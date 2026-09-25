//! Coverage for entity identity: the derivation itself, and the migration that
//! rewrites ids written by earlier builds.

use remind_me_core::entity::{
    entity_id, get_entity_by_id, get_entity_by_name, normalize_entity_name, renormalize_entity_ids,
    upsert_entity,
};
use remind_me_core::{Database, EntityInput};
use rusqlite::Connection;

fn input(name: &str, aliases: &[&str]) -> EntityInput {
    EntityInput {
        name: name.to_string(),
        kind: None,
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
    }
}

/// Insert a row the way an earlier build of this crate did: `ent_` plus the
/// full digest of a merely-trimmed name.
fn insert_legacy(conn: &Connection, name: &str, aliases: &str, created_at: &str) -> String {
    let id = format!("ent_{}", sha256::digest(name.trim().to_lowercase()));
    conn.execute(
        "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at)
         VALUES (?, ?, NULL, ?, ?, ?)",
        rusqlite::params![id, name.trim(), aliases, created_at, created_at],
    )
    .unwrap();
    id
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
    let conn = db.conn();

    let first = upsert_entity(&conn, &input("Bailey  Robertson", &["Bailey"])).unwrap();
    let second = upsert_entity(&conn, &input("Bailey Robertson", &["BR"])).unwrap();

    assert_eq!(first.id, second.id);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "the two spellings must not create two rows");
    assert_eq!(second.aliases, vec!["Bailey".to_string(), "BR".to_string()]);
}

#[test]
fn lookup_by_name_tolerates_casing_and_spacing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    upsert_entity(&conn, &input("Tasmania", &[])).unwrap();

    assert!(get_entity_by_name(&conn, "  TASMANIA ").unwrap().is_some());
}

#[test]
fn the_migration_rewrites_a_legacy_id_and_its_links() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let legacy = insert_legacy(&conn, "Tasmania", "[]", "2026-01-01T00:00:00Z");
    conn.execute(
        "INSERT INTO memory_entities (memory_id, entity_id, created_at)
         VALUES ('mem_1', ?, '2026-01-01T00:00:00Z')",
        rusqlite::params![legacy],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO entity_relations (id, subject_entity_id, relation, object_entity_id,
                                       created_at, updated_at)
         VALUES ('rel_1', ?, 'located_in', 'other', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        rusqlite::params![legacy],
    )
    .unwrap();

    assert_eq!(renormalize_entity_ids(&conn).unwrap(), 1);

    let want = entity_id("Tasmania");
    assert!(get_entity_by_id(&conn, &want).unwrap().is_some());
    assert!(get_entity_by_id(&conn, &legacy).unwrap().is_none());

    // Nothing cascades — there is no foreign key — so a link left pointing at
    // the old id would simply dangle, silently.
    let linked: String = conn
        .query_row(
            "SELECT entity_id FROM memory_entities WHERE memory_id = 'mem_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(linked, want);
    let subject: String = conn
        .query_row(
            "SELECT subject_entity_id FROM entity_relations WHERE id = 'rel_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(subject, want);
}

#[test]
fn the_migration_rewrites_object_side_relations_too() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let legacy = insert_legacy(&conn, "Hobart", "[]", "2026-01-01T00:00:00Z");
    conn.execute(
        "INSERT INTO entity_relations (id, subject_entity_id, relation, object_entity_id,
                                       created_at, updated_at)
         VALUES ('rel_1', 'other', 'capital_of', ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        rusqlite::params![legacy],
    )
    .unwrap();

    renormalize_entity_ids(&conn).unwrap();

    let object: String = conn
        .query_row(
            "SELECT object_entity_id FROM entity_relations WHERE id = 'rel_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(object, entity_id("Hobart"));
}

#[test]
fn the_migration_merges_rows_that_normalise_together() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Two rows only because the old derivation did not collapse internal runs.
    let spaced = insert_legacy(
        &conn,
        "Bailey  Robertson",
        r#"["Bailey"]"#,
        "2026-01-02T00:00:00Z",
    );
    let single = insert_legacy(
        &conn,
        "Bailey Robertson",
        r#"["BR"]"#,
        "2026-01-01T00:00:00Z",
    );
    for (memory, id) in [("mem_1", &spaced), ("mem_2", &single)] {
        conn.execute(
            "INSERT INTO memory_entities (memory_id, entity_id, created_at)
             VALUES (?, ?, '2026-01-01T00:00:00Z')",
            rusqlite::params![memory, id],
        )
        .unwrap();
    }

    renormalize_entity_ids(&conn).unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "colliding rows must merge, not fail the migration"
    );

    let merged = get_entity_by_id(&conn, &entity_id("Bailey Robertson"))
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
    let links: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_entities WHERE entity_id = ?",
            rusqlite::params![merged.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 2);
    let orphans: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_entities WHERE entity_id != ?",
            rusqlite::params![merged.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0, "no link may be left pointing at a dead id");
}

#[test]
fn a_duplicate_link_across_a_merge_collapses_rather_than_erroring() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let spaced = insert_legacy(&conn, "Bailey  Robertson", "[]", "2026-01-02T00:00:00Z");
    let single = insert_legacy(&conn, "Bailey Robertson", "[]", "2026-01-01T00:00:00Z");
    // The same memory mentions both — after the merge that is one link, and
    // `memory_entities` is keyed (memory_id, entity_id).
    for id in [&spaced, &single] {
        conn.execute(
            "INSERT INTO memory_entities (memory_id, entity_id, created_at)
             VALUES ('mem_1', ?, '2026-01-01T00:00:00Z')",
            rusqlite::params![id],
        )
        .unwrap();
    }

    renormalize_entity_ids(&conn).unwrap();

    let links: i64 = conn
        .query_row("SELECT count(*) FROM memory_entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(links, 1);
}

#[test]
fn the_migration_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    upsert_entity(&conn, &input("Tasmania", &["Tas"])).unwrap();

    assert_eq!(
        renormalize_entity_ids(&conn).unwrap(),
        0,
        "rows already on the current derivation must not be touched"
    );
    assert_eq!(renormalize_entity_ids(&conn).unwrap(), 0);
}

#[test]
fn opening_an_existing_database_migrates_it() {
    let dir = std::env::temp_dir().join(format!("rrm_entity_id_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("memories.db");
    let _ = std::fs::remove_file(&path);

    let legacy = {
        let db = Database::open(&path).unwrap();
        let conn = db.conn();
        insert_legacy(&conn, "Tasmania", "[]", "2026-01-01T00:00:00Z")
    };

    // Reopening runs the reconciler, which is where the rewrite lives.
    let db = Database::open(&path).unwrap();
    let conn = db.conn();
    assert!(get_entity_by_id(&conn, &legacy).unwrap().is_none());
    assert!(get_entity_by_name(&conn, "tasmania").unwrap().is_some());

    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}
