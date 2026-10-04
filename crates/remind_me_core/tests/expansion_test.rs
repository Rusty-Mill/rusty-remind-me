//! Coverage for the three search expansions and the co-retrieval write path.

use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::expansion::{
    record_co_retrieval, RelatedMemory, CO_RETRIEVAL_MAX_WEIGHT, CO_RETRIEVAL_PAIR_CAP,
    EXPANSION_CAP, SNIPPET_CHARS,
};
use remind_me_core::testing::{self, Table};
use remind_me_core::{Database, EntityInput, MemoryAddInput, MemorySearchInput};

fn add(store: &Store<'_>, content: &str, entities: &[&str]) -> String {
    queries::add_memory(
        store,
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
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

/// A memory carrying document position, the way an importer will write one.
fn chunk(store: &Store<'_>, id: &str, content: &str, doc: &str, index: i64) {
    // Written through the repository, which indexes it: searchable, and
    // deletable without the full-text index losing track of it.
    Memories::new(store)
        .insert(&NewMemory {
            source: "document_import".into(),
            doc_id: Some(doc.into()),
            chunk_index: Some(index),
            ..NewMemory::new(id, content, "2026-01-01T00:00:00Z")
        })
        .unwrap();
}

fn search(
    store: &Store<'_>,
    query: &str,
    configure: impl FnOnce(&mut MemorySearchInput),
) -> remind_me_core::expansion::MemorySearchResponse {
    let mut input = MemorySearchInput {
        scope: Default::default(),
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
        include_expired: true,
        min_confidence: 0.0,
    };
    configure(&mut input);
    queries::search_with_expansions(store, &input).unwrap()
}

fn ids(items: &[RelatedMemory]) -> Vec<String> {
    let mut out: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    out.sort();
    out
}

fn weight(store: &Store<'_>, a: &str, b: &str) -> Option<i64> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    testing::association_weight(store, lo, hi).unwrap()
}

// --- co-retrieval write path -------------------------------------------------

#[test]
fn pairs_are_stored_under_one_canonical_order() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    record_co_retrieval(&store, &["mem_b".into(), "mem_a".into()]).unwrap();
    record_co_retrieval(&store, &["mem_a".into(), "mem_b".into()]).unwrap();

    let rows = testing::count(&store, Table::MemoryAssociations).unwrap();
    // Without sorting, the two orderings would be two rows and each weight
    // would read back at half strength.
    assert_eq!(rows, 1);
    assert_eq!(weight(&store, "mem_a", "mem_b"), Some(2));
}

#[test]
fn repeated_co_retrieval_accumulates_and_clamps() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    for _ in 0..(CO_RETRIEVAL_MAX_WEIGHT + 20) {
        record_co_retrieval(&store, &["mem_a".into(), "mem_b".into()]).unwrap();
    }

    assert_eq!(
        weight(&store, "mem_a", "mem_b"),
        Some(CO_RETRIEVAL_MAX_WEIGHT)
    );
}

#[test]
fn fewer_than_two_results_record_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    assert_eq!(record_co_retrieval(&store, &[]).unwrap(), 0);
    assert_eq!(record_co_retrieval(&store, &["mem_a".into()]).unwrap(), 0);

    let rows = testing::count(&store, Table::MemoryAssociations).unwrap();
    assert_eq!(rows, 0, "a single result has nothing to associate with");
}

#[test]
fn only_the_first_ten_results_participate_in_pairing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let many: Vec<String> = (0..20).map(|i| format!("mem_{:02}", i)).collect();

    let touched = record_co_retrieval(&store, &many).unwrap();

    // Pairing is quadratic, so the cap is what bounds the writes one search
    // can produce: 10 * 9 / 2.
    let expected = CO_RETRIEVAL_PAIR_CAP * (CO_RETRIEVAL_PAIR_CAP - 1) / 2;
    assert_eq!(touched, expected);
    assert!(weight(&store, "mem_00", "mem_09").is_some());
    assert!(
        weight(&store, "mem_00", "mem_10").is_none(),
        "the eleventh result is outside the pairing cap"
    );
}

#[test]
fn searching_reinforces_associations_without_being_asked() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "quokka one", &[]);
    let b = add(&store, "quokka two", &[]);

    // expand_co_retrieval is false: surfacing is opt-in, recording is not. A
    // graph that only filled when someone was looking would never have
    // anything to show.
    search(&store, "quokka", |_| {});

    assert_eq!(weight(&store, &a, &b), Some(1));
}

#[test]
fn deleting_a_memory_clears_associations_on_either_side() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "quokka one", &[]);
    let b = add(&store, "quokka two", &[]);
    let c = add(&store, "quokka three", &[]);
    search(&store, "quokka", |_| {});
    assert!(weight(&store, &a, &b).is_some());

    queries::delete_memory(&store, &b).unwrap();

    // There is no foreign key here — the reference omits it so sync can deliver
    // rows out of order — so this relies on delete_memory cleaning up, and it
    // must cover the pair whichever side b sorted onto.
    assert!(weight(&store, &a, &b).is_none());
    assert!(weight(&store, &b, &c).is_none());
    assert!(weight(&store, &a, &c).is_some(), "unrelated pairs survive");
}

// --- expansions are opt-in ---------------------------------------------------

#[test]
fn every_expansion_is_absent_unless_requested() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka one", &["Tasmania"]);
    add(&store, "quokka two", &["Tasmania"]);

    let result = search(&store, "quokka", |_| {});

    assert!(result.related_via_entities.is_none());
    assert!(result.related_via_neighbors.is_none());
    assert!(result.related_via_co_retrieval.is_none());
}

// --- entity expansion --------------------------------------------------------

#[test]
fn entity_expansion_finds_memories_sharing_an_entity() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting", &["Tasmania"]);
    let neighbour = add(&store, "unrelated wording entirely", &["Tasmania"]);
    add(&store, "nothing in common", &["Fiji"]);

    let result = search(&store, "quokka", |i| i.expand_entities = true);

    let related = result.related_via_entities.unwrap();
    assert_eq!(ids(&related), vec![neighbour]);
    assert_eq!(related[0].via_entities, vec!["Tasmania".to_string()]);
}

#[test]
fn entity_expansion_excludes_the_seeds_themselves() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka one", &["Tasmania"]);
    add(&store, "quokka two", &["Tasmania"]);

    let result = search(&store, "quokka", |i| i.expand_entities = true);

    // Both memories match the query, so both are seeds — there is no third
    // memory to expand to.
    assert!(result.related_via_entities.unwrap().is_empty());
}

#[test]
fn entity_expansion_gathers_multiple_links_onto_one_item() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting", &["Tasmania", "Hobart"]);
    let neighbour = add(&store, "unrelated wording", &["Tasmania", "Hobart"]);

    let result = search(&store, "quokka", |i| i.expand_entities = true);

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
    let store = db.store();
    add(&store, "quokka sighting", &["Tasmania"]);
    let live = add(&store, "still around", &["Tasmania"]);
    let superseded = add(&store, "replaced note", &["Tasmania"]);
    testing::set_memory_column(&store, &superseded, "superseded_by", live.as_str()).unwrap();

    let result = search(&store, "quokka", |i| i.expand_entities = true);

    assert_eq!(ids(&result.related_via_entities.unwrap()), vec![live]);
}

#[test]
fn entity_expansion_is_capped() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting", &["Tasmania"]);
    for i in 0..(EXPANSION_CAP + 4) {
        add(&store, &format!("unrelated wording {}", i), &["Tasmania"]);
    }

    let result = search(&store, "quokka", |i| i.expand_entities = true);

    assert_eq!(result.related_via_entities.unwrap().len(), EXPANSION_CAP);
}

// --- document-neighbour expansion --------------------------------------------

#[test]
fn neighbor_expansion_finds_adjacent_chunks() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    chunk(&store, "mem_0", "opening paragraph", "doc_1", 0);
    chunk(&store, "mem_1", "quokka paragraph", "doc_1", 1);
    chunk(&store, "mem_2", "closing paragraph", "doc_1", 2);
    chunk(&store, "mem_3", "far away paragraph", "doc_1", 9);
    chunk(&store, "mem_x", "other document", "doc_2", 1);

    let result = search(&store, "quokka", |i| i.include_neighbors = true);

    // Window is one position either side, same document only.
    assert_eq!(
        ids(&result.related_via_neighbors.unwrap()),
        vec!["mem_0".to_string(), "mem_2".to_string()]
    );
}

#[test]
fn neighbor_expansion_carries_the_document_position() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    chunk(&store, "mem_0", "opening paragraph", "doc_1", 0);
    chunk(&store, "mem_1", "quokka paragraph", "doc_1", 1);

    let result = search(&store, "quokka", |i| i.include_neighbors = true);

    let related = result.related_via_neighbors.unwrap();
    assert_eq!(related[0].doc_id.as_deref(), Some("doc_1"));
    assert_eq!(related[0].chunk_index, Some(0));
}

#[test]
fn neighbor_expansion_is_empty_without_a_document() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting", &[]);
    add(&store, "quokka again", &[]);

    let result = search(&store, "quokka", |i| i.include_neighbors = true);

    // A manually added memory is not part of a document, so it has no
    // siblings. On a store with no importers this always comes back empty.
    assert!(result.related_via_neighbors.unwrap().is_empty());
}

// --- co-retrieval expansion --------------------------------------------------

#[test]
fn co_retrieval_expansion_surfaces_past_companions() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let quokka = add(&store, "quokka sighting", &[]);
    let companion = add(&store, "quokka companion note", &[]);
    // Both come back for "quokka", so searching once associates them.
    search(&store, "quokka", |_| {});

    // Now search for something only the first matches: the companion should be
    // surfaced by association rather than by the query.
    let result = search(&store, "sighting", |i| i.expand_co_retrieval = true);

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
    let store = db.store();
    let seed = add(&store, "quokka sighting", &[]);
    let often = add(&store, "often together", &[]);
    let once = add(&store, "once together", &[]);
    record_co_retrieval(&store, &[seed.clone(), once.clone()]).unwrap();
    for _ in 0..5 {
        record_co_retrieval(&store, &[seed.clone(), often.clone()]).unwrap();
    }

    let result = search(&store, "sighting", |i| i.expand_co_retrieval = true);

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
    let store = db.store();
    let seed = add(&store, "quokka sighting", &[]);
    let other = add(&store, "the companion", &[]);
    record_co_retrieval(&store, &[seed.clone(), other.clone()]).unwrap();

    let result = search(&store, "sighting", |i| i.expand_co_retrieval = true);

    // A pair is stored once under a canonical order, so the seed may sit on
    // either side and the read has to cover both.
    assert_eq!(ids(&result.related_via_co_retrieval.unwrap()), vec![other]);
}

#[test]
fn co_retrieval_expansion_is_capped_and_snippets_are_trimmed() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let seed = add(&store, "quokka sighting", &[]);
    for i in 0..(EXPANSION_CAP + 4) {
        let other = add(&store, &format!("{} companion {}", "x".repeat(500), i), &[]);
        record_co_retrieval(&store, &[seed.clone(), other]).unwrap();
    }

    let result = search(&store, "sighting", |i| i.expand_co_retrieval = true);

    let related = result.related_via_co_retrieval.unwrap();
    assert_eq!(related.len(), EXPANSION_CAP);
    assert_eq!(related[0].content_snippet.chars().count(), SNIPPET_CHARS);
}

// --- expansions stay outside the ranking -------------------------------------

#[test]
fn expansions_do_not_consume_the_limit() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting", &["Tasmania"]);
    for i in 0..4 {
        add(&store, &format!("unrelated wording {}", i), &["Tasmania"]);
    }

    let result = search(&store, "quokka", |i| {
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
    let store = db.store();
    let first = add(&store, "quokka alpha", &[]);
    let second = add(&store, "quokka beta", &[]);
    let before: Vec<String> = search(&store, "quokka", |_| {})
        .memories
        .iter()
        .map(|r| r.memory.id.clone())
        .collect();

    for _ in 0..CO_RETRIEVAL_MAX_WEIGHT {
        record_co_retrieval(&store, &[first.clone(), second.clone()]).unwrap();
    }

    let after: Vec<String> = search(&store, "quokka", |i| i.expand_co_retrieval = true)
        .memories
        .iter()
        .map(|r| r.memory.id.clone())
        .collect();

    // Letting a recorded weight feed the ranking would build a loop where
    // whatever was returned together once is returned together forever. The
    // maxed-out association must leave the order untouched.
    assert_eq!(before, after);
}
