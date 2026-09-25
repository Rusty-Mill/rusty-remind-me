//! Coverage for `remind_me_decompose` / `remind_me_decompose_batch`, including
//! contradiction-based supersession and the relation edges decomposition writes.

use remind_me_core::capture::{auto_capture, decompose, undecomposed_batch};
use remind_me_core::db::queries;
use remind_me_core::entity::{entity_id, traverse_entities, upsert_entity};
use remind_me_core::vitality::get_decay_rate;
use remind_me_core::{
    AtomicFact, AutoCaptureInput, CaptureResult, Database, DecomposeBatchInput, DecomposeInput,
    DecomposeResult, EntityInput, DECOMPOSE_FACTS_MAX, DECOMPOSITION_SOURCE, FACT_CATEGORY,
    UNCLASSIFIED,
};
use rusqlite::Connection;

fn capture(conn: &Connection, tags: &[&str]) -> CaptureResult {
    auto_capture(
        conn,
        &AutoCaptureInput {
            conversation: "user: where do you live\nassistant: Seattle".into(),
            summary: "a conversation about where I live".into(),
            title: String::new(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            category: "conversation".into(),
            metadata: serde_json::json!({}),
        },
    )
    .unwrap()
}

fn fact(content: &str) -> AtomicFact {
    AtomicFact {
        content: content.to_string(),
        memory_type: None,
        extra_tags: vec![],
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
    }
}

fn triple(content: &str, subject: &str, predicate: &str, object: &str) -> AtomicFact {
    AtomicFact {
        subject: Some(subject.into()),
        predicate: Some(predicate.into()),
        object: Some(object.into()),
        ..fact(content)
    }
}

fn run(conn: &Connection, capture_id: &str, facts: Vec<AtomicFact>) -> DecomposeResult {
    decompose(
        conn,
        &DecomposeInput {
            capture_id: capture_id.to_string(),
            facts,
        },
    )
    .unwrap()
    .unwrap()
}

fn column(conn: &Connection, id: &str, name: &str) -> String {
    conn.query_row(
        &format!("SELECT {} FROM memories WHERE id = ?", name),
        rusqlite::params![id],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

fn known(conn: &Connection, name: &str) {
    upsert_entity(
        conn,
        &EntityInput {
            name: name.to_string(),
            kind: None,
            aliases: vec![],
        },
    )
    .unwrap();
}

#[test]
fn facts_are_written_and_linked_to_their_capture() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);

    let result = run(&conn, &parent.capture_id, vec![fact("I live in Seattle")]);

    assert_eq!(result.created, 1);
    let id = &result.fact_ids[0];
    assert_eq!(column(&conn, id, "content"), "I live in Seattle");
    assert_eq!(column(&conn, id, "category"), FACT_CATEGORY);
    assert_eq!(column(&conn, id, "source"), DECOMPOSITION_SOURCE);
    assert_eq!(column(&conn, id, "source_capture_id"), parent.capture_id);
}

#[test]
fn a_fact_is_not_itself_a_capture() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);

    let result = run(&conn, &parent.capture_id, vec![fact("a fact")]);

    let capture_id: Option<String> = conn
        .query_row(
            "SELECT capture_id FROM memories WHERE id = ?",
            rusqlite::params![result.fact_ids[0]],
            |r| r.get(0),
        )
        .unwrap();
    // A fact carrying a capture_id would re-enter the decomposition backlog,
    // so decomposition would generate its own work forever.
    assert!(capture_id.is_none());
}

#[test]
fn facts_inherit_the_parents_tags_merged_with_their_own() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &["session", "shared"]);

    let mut extra = fact("a fact");
    extra.extra_tags = vec!["shared".into(), "specific".into()];
    let result = run(&conn, &parent.capture_id, vec![extra]);

    let tags: Vec<String> =
        serde_json::from_str(&column(&conn, &result.fact_ids[0], "tags")).unwrap();
    // Parent first, then the fact's own, de-duplicated and order-preserving.
    assert_eq!(tags, vec!["session", "shared", "specific"]);
    assert_eq!(result.parent_tags_inherited, vec!["session", "shared"]);
}

#[test]
fn the_memory_type_drives_the_decay_rate_and_weight() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);

    let mut decision = fact("we chose SQLite");
    decision.memory_type = Some("decision".into());
    let result = run(&conn, &parent.capture_id, vec![decision]);

    let id = &result.fact_ids[0];
    assert_eq!(column(&conn, id, "memory_type"), "decision");
    let (decay, weight, vitality): (f64, f64, f64) = conn
        .query_row(
            "SELECT decay_rate, base_weight, vitality FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!((decay - get_decay_rate("decision")).abs() < 1e-9);
    // A decision outranks an unclassified aside before any feedback exists. At
    // zero elapsed days vitality equals base_weight exactly.
    assert!(weight > 1.0);
    assert!((vitality - weight).abs() < 1e-9);
}

#[test]
fn an_unspecified_memory_type_is_unclassified() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);

    let result = run(&conn, &parent.capture_id, vec![fact("a fact")]);

    assert_eq!(
        column(&conn, &result.fact_ids[0], "memory_type"),
        UNCLASSIFIED
    );
}

#[test]
fn an_unknown_capture_id_reports_not_found() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let outcome = decompose(
        &conn,
        &DecomposeInput {
            capture_id: "cap_nope".into(),
            facts: vec![fact("orphan")],
        },
    )
    .unwrap();

    assert!(outcome.is_none());
}

#[test]
fn a_full_batch_of_facts_applies() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);

    let facts: Vec<AtomicFact> = (0..DECOMPOSE_FACTS_MAX)
        .map(|i| fact(&format!("fact {}", i)))
        .collect();
    let result = run(&conn, &parent.capture_id, facts);

    assert_eq!(result.created, DECOMPOSE_FACTS_MAX);
}

// --- contradiction supersession ---------------------------------------------

#[test]
fn a_contradicting_triple_supersedes_the_older_fact() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    let first = run(
        &conn,
        &parent.capture_id,
        vec![triple("I live in Seattle", "Bailey", "lives_in", "Seattle")],
    );

    let second = run(
        &conn,
        &parent.capture_id,
        vec![triple("I moved to Boston", "Bailey", "lives_in", "Boston")],
    );

    // The two share no words, so similarity-based merging could never catch
    // this. Same subject and predicate, different object, is the signal.
    assert_eq!(second.superseded_ids, first.fact_ids);
    assert_eq!(
        column(&conn, &first.fact_ids[0], "superseded_by"),
        second.fact_ids[0]
    );
}

#[test]
fn restating_the_same_fact_is_not_a_contradiction() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    run(
        &conn,
        &parent.capture_id,
        vec![triple("I live in Seattle", "Bailey", "lives_in", "Seattle")],
    );

    let again = run(
        &conn,
        &parent.capture_id,
        vec![triple("Still Seattle", "bailey", "Lives_In", "  seattle ")],
    );

    // Same object, so the same fact restated — casing and spacing aside.
    assert!(again.superseded_ids.is_empty());
}

#[test]
fn a_different_predicate_is_not_a_contradiction() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    run(
        &conn,
        &parent.capture_id,
        vec![triple("I live in Seattle", "Bailey", "lives_in", "Seattle")],
    );

    let visited = run(
        &conn,
        &parent.capture_id,
        vec![triple("I visited Boston", "Bailey", "visited", "Boston")],
    );

    // This is deliberately not predicate inference: "visited" says nothing
    // about where someone lives.
    assert!(visited.superseded_ids.is_empty());
}

#[test]
fn an_incomplete_triple_supersedes_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    run(
        &conn,
        &parent.capture_id,
        vec![triple("I live in Seattle", "Bailey", "lives_in", "Seattle")],
    );

    let mut partial = fact("something vague");
    partial.subject = Some("Bailey".into());
    partial.predicate = Some("lives_in".into());
    let result = run(&conn, &parent.capture_id, vec![partial]);

    assert!(result.superseded_ids.is_empty());
}

#[test]
fn a_superseded_fact_drops_out_of_search() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    run(
        &conn,
        &parent.capture_id,
        vec![triple(
            "quokka lives in Seattle",
            "Quokka",
            "lives_in",
            "Seattle",
        )],
    );
    let second = run(
        &conn,
        &parent.capture_id,
        vec![triple(
            "quokka moved to Boston",
            "Quokka",
            "lives_in",
            "Boston",
        )],
    );

    let found: Vec<String> = queries::search_memories(
        &conn,
        &remind_me_core::MemorySearchInput {
            strategy: Default::default(),
            include_sensitive: false,
            query: "quokka".into(),
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
        },
    )
    .unwrap()
    .into_iter()
    .map(|r| r.memory.id)
    .collect();

    // The whole point of supersession is that the replaced fact stops being
    // returned. Search excluded only `deleted_at` before this change, which was
    // harmless while nothing ever set `superseded_by` — and would have made
    // "I live in Seattle" keep coming back after "I moved to Boston" replaced
    // it.
    assert_eq!(found, second.fact_ids);
}

// --- relation edges ----------------------------------------------------------

#[test]
fn a_triple_naming_two_known_entities_records_an_edge() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    known(&conn, "Bailey");
    known(&conn, "Seattle");

    let result = run(
        &conn,
        &parent.capture_id,
        vec![triple("I live in Seattle", "Bailey", "lives_in", "Seattle")],
    );

    assert_eq!(result.relations_linked, 1);
    let edges = traverse_entities(&conn, &[entity_id("Bailey")], 1, None, 20).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].relation, "lives_in");
    assert_eq!(edges[0].object_name, "Seattle");
}

#[test]
fn entities_named_by_the_fact_itself_resolve_for_the_edge() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);

    let mut with_entities = triple("I live in Seattle", "Bailey", "lives_in", "Seattle");
    with_entities.entities = vec![
        EntityInput {
            name: "Bailey".into(),
            kind: None,
            aliases: vec![],
        },
        EntityInput {
            name: "Seattle".into(),
            kind: None,
            aliases: vec![],
        },
    ];
    let result = run(&conn, &parent.capture_id, vec![with_entities]);

    // The mentions are applied before the edge is attempted, so the entities
    // this same fact names are already known by the time it resolves them.
    // Reversing that order would find nothing.
    assert_eq!(result.entities_linked, 2);
    assert_eq!(result.relations_linked, 1);
}

#[test]
fn a_triple_naming_an_unknown_entity_records_no_edge() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    known(&conn, "Bailey");

    let result = run(
        &conn,
        &parent.capture_id,
        vec![triple(
            "I live in Atlantis",
            "Bailey",
            "lives_in",
            "Atlantis",
        )],
    );

    // A triple is free text; writing one does not imply it names anything in
    // the graph. The memory-level triple still stands.
    assert_eq!(result.relations_linked, 0);
    assert_eq!(column(&conn, &result.fact_ids[0], "object"), "Atlantis");
}

#[test]
fn repeating_an_edge_does_not_duplicate_it() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    known(&conn, "Bailey");
    known(&conn, "Seattle");

    for _ in 0..3 {
        run(
            &conn,
            &parent.capture_id,
            vec![triple("I live in Seattle", "Bailey", "lives_in", "Seattle")],
        );
    }

    let edges: i64 = conn
        .query_row("SELECT count(*) FROM entity_relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        edges, 1,
        "the edge id is derived, so re-recording is a no-op"
    );
}

// --- the batch ---------------------------------------------------------------

#[test]
fn a_fresh_capture_awaits_decomposition() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &["session"]);

    let batch = undecomposed_batch(&conn, &DecomposeBatchInput { batch_size: 20 }).unwrap();

    // Both halves of the capture carry the capture_id, so both are offered.
    assert_eq!(batch.total_undecomposed, 2);
    assert!(batch.memories.iter().any(|m| m.id == parent.dialog_id));
    assert_eq!(batch.memories[0].capture_id, parent.capture_id);
    assert_eq!(batch.memories[0].tags, vec!["session".to_string()]);
}

#[test]
fn a_decomposed_capture_drops_out_of_the_batch() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let parent = capture(&conn, &[]);
    assert_eq!(
        undecomposed_batch(&conn, &DecomposeBatchInput { batch_size: 20 })
            .unwrap()
            .total_undecomposed,
        2
    );

    run(&conn, &parent.capture_id, vec![fact("a fact")]);

    // There is no decomposed flag — the backlog shrinks because a fact now
    // names this capture as its source.
    assert_eq!(
        undecomposed_batch(&conn, &DecomposeBatchInput { batch_size: 20 })
            .unwrap()
            .total_undecomposed,
        0
    );
}

#[test]
fn ordinary_memories_never_enter_the_batch() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    queries::add_memory(
        &conn,
        remind_me_core::MemoryAddInput {
            sensitive: false,
            content: "written by hand".into(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();

    let batch = undecomposed_batch(&conn, &DecomposeBatchInput { batch_size: 20 }).unwrap();

    assert_eq!(batch.total_undecomposed, 0);
}

#[test]
fn the_batch_size_is_clamped_and_the_backlog_is_reported() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for _ in 0..3 {
        capture(&conn, &[]);
    }

    let page = undecomposed_batch(&conn, &DecomposeBatchInput { batch_size: 2 }).unwrap();
    assert_eq!(page.memories.len(), 2);
    assert_eq!(page.total_undecomposed, 6);

    assert_eq!(
        undecomposed_batch(&conn, &DecomposeBatchInput { batch_size: 0 })
            .unwrap()
            .memories
            .len(),
        1,
        "zero clamps up to 1"
    );
}
