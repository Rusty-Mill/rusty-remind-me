//! Coverage for the three search expansions and the co-retrieval write path.

use remind_me_core::db::queries;
use remind_me_core::expansion::{
    record_co_retrieval, RelatedMemory, CO_RETRIEVAL_MAX_WEIGHT, CO_RETRIEVAL_PAIR_CAP,
    EXPANSION_CAP, SNIPPET_CHARS,
};
use remind_me_core::{Database, EntityInput, MemoryAddInput, MemorySearchInput};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, entities: &[&str]) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "fact".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: entities
                .iter()
                .map(|n| EntityInput {
                    name: n.to_string(),
                    kind: None,
                    aliases: vec![],
                })
                .collect(),
        },
    )
    .unwrap()
    .id
}

/// A memory carrying document position, the way an importer will write one.
fn chunk(conn: &Connection, id: &str, content: &str, doc: &str, index: i64) {
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata,
                               created_at, updated_at, doc_id, chunk_index)
         VALUES (?, ?, 'general', '[]', 'document_import', '{}',
                 '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', ?, ?)",
        rusqlite::params![id, content, doc, index],
    )
    .unwrap();
}

fn search(
    conn: &Connection,
    query: &str,
    configure: impl FnOnce(&mut MemorySearchInput),
) -> remind_me_core::expansion::MemorySearchResponse {
    let mut input = MemorySearchInput {
        strategy: Default::default(),
        include_sensitive: false,
        query: query.to_string(),
        category: None,
        tags: None,
        limit: 20,
        token_budget: 100_000,
        response_format: Default::default(),
        include_dormant: true,
        min_vitality: 0.0,
        verbose: false,
        expand_entities: false,
        include_neighbors: false,
        expand_co_retrieval: false,
        bootstrap: false,
    };
    configure(&mut input);
    queries::search_with_expansions(conn, &input).unwrap()
}

fn ids(items: &[RelatedMemory]) -> Vec<String> {
    let mut out: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    out.sort();
    out
}

fn weight(conn: &Connection, a: &str, b: &str) -> Option<i64> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    conn.query_row(
        "SELECT weight FROM memory_associations WHERE memory_id_a = ? AND memory_id_b = ?",
        rusqlite::params![lo, hi],
        |r| r.get(0),
    )
    .ok()
}

// --- co-retrieval write path -------------------------------------------------

#[test]
fn pairs_are_stored_under_one_canonical_order() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    record_co_retrieval(&conn, &["mem_b".into(), "mem_a".into()]).unwrap();
    record_co_retrieval(&conn, &["mem_a".into(), "mem_b".into()]).unwrap();

    let rows: i64 = conn
        .query_row("SELECT count(*) FROM memory_associations", [], |r| r.get(0))
        .unwrap();
    // Without sorting, the two orderings would be two rows and each weight
    // would read back at half strength.
    assert_eq!(rows, 1);
    assert_eq!(weight(&conn, "mem_a", "mem_b"), Some(2));
}

#[test]
fn repeated_co_retrieval_accumulates_and_clamps() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    for _ in 0..(CO_RETRIEVAL_MAX_WEIGHT + 20) {
        record_co_retrieval(&conn, &["mem_a".into(), "mem_b".into()]).unwrap();
    }

    assert_eq!(
        weight(&conn, "mem_a", "mem_b"),
        Some(CO_RETRIEVAL_MAX_WEIGHT)
    );
}

#[test]
fn fewer_than_two_results_record_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    assert_eq!(record_co_retrieval(&conn, &[]).unwrap(), 0);
    assert_eq!(record_co_retrieval(&conn, &["mem_a".into()]).unwrap(), 0);

    let rows: i64 = conn
        .query_row("SELECT count(*) FROM memory_associations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "a single result has nothing to associate with");
}

#[test]
fn only_the_first_ten_results_participate_in_pairing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let many: Vec<String> = (0..20).map(|i| format!("mem_{:02}", i)).collect();

    let touched = record_co_retrieval(&conn, &many).unwrap();

    // Pairing is quadratic, so the cap is what bounds the writes one search
    // can produce: 10 * 9 / 2.
    let expected = CO_RETRIEVAL_PAIR_CAP * (CO_RETRIEVAL_PAIR_CAP - 1) / 2;
    assert_eq!(touched, expected);
    assert!(weight(&conn, "mem_00", "mem_09").is_some());
    assert!(
        weight(&conn, "mem_00", "mem_10").is_none(),
        "the eleventh result is outside the pairing cap"
    );
}

#[test]
fn searching_reinforces_associations_without_being_asked() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add(&conn, "quokka one", &[]);
    let b = add(&conn, "quokka two", &[]);

    // expand_co_retrieval is false: surfacing is opt-in, recording is not. A
    // graph that only filled when someone was looking would never have
    // anything to show.
    search(&conn, "quokka", |_| {});

    assert_eq!(weight(&conn, &a, &b), Some(1));
}

#[test]
fn deleting_a_memory_clears_associations_on_either_side() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add(&conn, "quokka one", &[]);
    let b = add(&conn, "quokka two", &[]);
    let c = add(&conn, "quokka three", &[]);
    search(&conn, "quokka", |_| {});
    assert!(weight(&conn, &a, &b).is_some());

    queries::delete_memory(&conn, &b).unwrap();

    // There is no foreign key here — the reference omits it so sync can deliver
    // rows out of order — so this relies on delete_memory cleaning up, and it
    // must cover the pair whichever side b sorted onto.
    assert!(weight(&conn, &a, &b).is_none());
    assert!(weight(&conn, &b, &c).is_none());
    assert!(weight(&conn, &a, &c).is_some(), "unrelated pairs survive");
}

// --- expansions are opt-in ---------------------------------------------------

#[test]
fn every_expansion_is_absent_unless_requested() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka one", &["Tasmania"]);
    add(&conn, "quokka two", &["Tasmania"]);

    let result = search(&conn, "quokka", |_| {});

    assert!(result.related_via_entities.is_none());
    assert!(result.related_via_neighbors.is_none());
    assert!(result.related_via_co_retrieval.is_none());
}

// --- entity expansion --------------------------------------------------------

#[test]
fn entity_expansion_finds_memories_sharing_an_entity() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka sighting", &["Tasmania"]);
    let neighbour = add(&conn, "unrelated wording entirely", &["Tasmania"]);
    add(&conn, "nothing in common", &["Fiji"]);

    let result = search(&conn, "quokka", |i| i.expand_entities = true);

    let related = result.related_via_entities.unwrap();
    assert_eq!(ids(&related), vec![neighbour]);
    assert_eq!(related[0].via_entities, vec!["Tasmania".to_string()]);
}

#[test]
fn entity_expansion_excludes_the_seeds_themselves() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka one", &["Tasmania"]);
    add(&conn, "quokka two", &["Tasmania"]);

    let result = search(&conn, "quokka", |i| i.expand_entities = true);

    // Both memories match the query, so both are seeds — there is no third
    // memory to expand to.
    assert!(result.related_via_entities.unwrap().is_empty());
}

#[test]
fn entity_expansion_gathers_multiple_links_onto_one_item() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka sighting", &["Tasmania", "Hobart"]);
    let neighbour = add(&conn, "unrelated wording", &["Tasmania", "Hobart"]);

    let result = search(&conn, "quokka", |i| i.expand_entities = true);

    let related = result.related_via_entities.unwrap();
    // One row per (memory, entity) pair comes back from SQL; the memory must
    // appear once with both names, not twice.
    assert_eq!(related.len(), 1);
    assert_eq!(related[0].id, neighbour);
    let mut via = related[0].via_entities.clone();
    via.sort();
    assert_eq!(via, vec!["Hobart".to_string(), "Tasmania".to_string()]);
}

#[test]
fn entity_expansion_skips_deleted_and_superseded_memories() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka sighting", &["Tasmania"]);
    let live = add(&conn, "still around", &["Tasmania"]);
    let superseded = add(&conn, "replaced note", &["Tasmania"]);
    conn.execute(
        "UPDATE memories SET superseded_by = ? WHERE id = ?",
        rusqlite::params![live, superseded],
    )
    .unwrap();

    let result = search(&conn, "quokka", |i| i.expand_entities = true);

    assert_eq!(ids(&result.related_via_entities.unwrap()), vec![live]);
}

#[test]
fn entity_expansion_is_capped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka sighting", &["Tasmania"]);
    for i in 0..(EXPANSION_CAP + 4) {
        add(&conn, &format!("unrelated wording {}", i), &["Tasmania"]);
    }

    let result = search(&conn, "quokka", |i| i.expand_entities = true);

    assert_eq!(result.related_via_entities.unwrap().len(), EXPANSION_CAP);
}

// --- document-neighbour expansion --------------------------------------------

#[test]
fn neighbor_expansion_finds_adjacent_chunks() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    chunk(&conn, "mem_0", "opening paragraph", "doc_1", 0);
    chunk(&conn, "mem_1", "quokka paragraph", "doc_1", 1);
    chunk(&conn, "mem_2", "closing paragraph", "doc_1", 2);
    chunk(&conn, "mem_3", "far away paragraph", "doc_1", 9);
    chunk(&conn, "mem_x", "other document", "doc_2", 1);

    let result = search(&conn, "quokka", |i| i.include_neighbors = true);

    // Window is one position either side, same document only.
    assert_eq!(
        ids(&result.related_via_neighbors.unwrap()),
        vec!["mem_0".to_string(), "mem_2".to_string()]
    );
}

#[test]
fn neighbor_expansion_carries_the_document_position() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    chunk(&conn, "mem_0", "opening paragraph", "doc_1", 0);
    chunk(&conn, "mem_1", "quokka paragraph", "doc_1", 1);

    let result = search(&conn, "quokka", |i| i.include_neighbors = true);

    let related = result.related_via_neighbors.unwrap();
    assert_eq!(related[0].doc_id.as_deref(), Some("doc_1"));
    assert_eq!(related[0].chunk_index, Some(0));
}

#[test]
fn neighbor_expansion_is_empty_without_a_document() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka sighting", &[]);
    add(&conn, "quokka again", &[]);

    let result = search(&conn, "quokka", |i| i.include_neighbors = true);

    // A manually added memory is not part of a document, so it has no
    // siblings. On a store with no importers this always comes back empty.
    assert!(result.related_via_neighbors.unwrap().is_empty());
}

// --- co-retrieval expansion --------------------------------------------------

#[test]
fn co_retrieval_expansion_surfaces_past_companions() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let quokka = add(&conn, "quokka sighting", &[]);
    let companion = add(&conn, "quokka companion note", &[]);
    // Both come back for "quokka", so searching once associates them.
    search(&conn, "quokka", |_| {});

    // Now search for something only the first matches: the companion should be
    // surfaced by association rather than by the query.
    let result = search(&conn, "sighting", |i| i.expand_co_retrieval = true);

    assert_eq!(
        result
            .memories
            .iter()
            .map(|r| r.memory.id.clone())
            .collect::<Vec<_>>(),
        vec![quokka]
    );
    let related = result.related_via_co_retrieval.unwrap();
    assert_eq!(ids(&related), vec![companion]);
    assert_eq!(related[0].co_retrieval_weight, Some(1));
}

#[test]
fn co_retrieval_expansion_orders_by_weight() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let seed = add(&conn, "quokka sighting", &[]);
    let often = add(&conn, "often together", &[]);
    let once = add(&conn, "once together", &[]);
    record_co_retrieval(&conn, &[seed.clone(), once.clone()]).unwrap();
    for _ in 0..5 {
        record_co_retrieval(&conn, &[seed.clone(), often.clone()]).unwrap();
    }

    let result = search(&conn, "sighting", |i| i.expand_co_retrieval = true);

    let related = result.related_via_co_retrieval.unwrap();
    assert_eq!(
        related.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        vec![often, once],
        "strongest association first"
    );
}

#[test]
fn co_retrieval_expansion_reads_both_sides_of_a_pair() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let seed = add(&conn, "quokka sighting", &[]);
    let other = add(&conn, "the companion", &[]);
    record_co_retrieval(&conn, &[seed.clone(), other.clone()]).unwrap();

    let result = search(&conn, "sighting", |i| i.expand_co_retrieval = true);

    // A pair is stored once under a canonical order, so the seed may sit on
    // either side and the read has to cover both.
    assert_eq!(ids(&result.related_via_co_retrieval.unwrap()), vec![other]);
}

#[test]
fn co_retrieval_expansion_is_capped_and_snippets_are_trimmed() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let seed = add(&conn, "quokka sighting", &[]);
    for i in 0..(EXPANSION_CAP + 4) {
        let other = add(&conn, &format!("{} companion {}", "x".repeat(500), i), &[]);
        record_co_retrieval(&conn, &[seed.clone(), other]).unwrap();
    }

    let result = search(&conn, "sighting", |i| i.expand_co_retrieval = true);

    let related = result.related_via_co_retrieval.unwrap();
    assert_eq!(related.len(), EXPANSION_CAP);
    assert_eq!(related[0].content_snippet.chars().count(), SNIPPET_CHARS);
}

// --- expansions stay outside the ranking -------------------------------------

#[test]
fn expansions_do_not_consume_the_limit() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "quokka sighting", &["Tasmania"]);
    for i in 0..4 {
        add(&conn, &format!("unrelated wording {}", i), &["Tasmania"]);
    }

    let result = search(&conn, "quokka", |i| {
        i.limit = 1;
        i.expand_entities = true;
    });

    // The ranked list honours limit; the expansion sits outside it and is
    // bounded by its own cap instead.
    assert_eq!(result.memories.len(), 1);
    assert_eq!(result.related_via_entities.unwrap().len(), 4);
}

#[test]
fn co_retrieval_weight_never_reaches_the_ranking() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let first = add(&conn, "quokka alpha", &[]);
    let second = add(&conn, "quokka beta", &[]);
    let before: Vec<String> = search(&conn, "quokka", |_| {})
        .memories
        .iter()
        .map(|r| r.memory.id.clone())
        .collect();

    for _ in 0..CO_RETRIEVAL_MAX_WEIGHT {
        record_co_retrieval(&conn, &[first.clone(), second.clone()]).unwrap();
    }

    let after: Vec<String> = search(&conn, "quokka", |i| i.expand_co_retrieval = true)
        .memories
        .iter()
        .map(|r| r.memory.id.clone())
        .collect();

    // Letting a recorded weight feed the ranking would build a loop where
    // whatever was returned together once is returned together forever. The
    // maxed-out association must leave the order untouched.
    assert_eq!(before, after);
}
