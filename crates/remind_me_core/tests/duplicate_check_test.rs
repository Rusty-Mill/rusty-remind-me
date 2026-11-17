//! The near-duplicate check on save (Mill Board #1486).

use remind_me_core::capture::{auto_capture, decompose};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries;
use remind_me_core::db::{Store, StoreError};
use remind_me_core::duplicates::{check_on_save, duplicate_check};
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::vectors::embed_and_store;
use remind_me_core::{
    AtomicFact, AutoCaptureInput, Database, DecomposeInput, DuplicateReason, MemoryAddInput,
};

/// Maps each text to a fixed direction: texts naming a cat point one way,
/// everything else another, so a paraphrase with no shared words still
/// embeds close by.
struct TopicEmbedder;

impl Embedder for TopicEmbedder {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                let t = t.to_lowercase();
                if t.contains("cat") || t.contains("feline") {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                }
            })
            .collect())
    }

    fn dim(&self) -> usize {
        2
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "topic".into(),
            model: "test".into(),
            dim: 2,
        }
    }
}

fn add_in(store: &Store<'_>, content: &str, project: &str, sensitive: bool) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            project: Some(project.to_string()),
            sensitive,
            extract: false,
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

/// A memory with no project, inserted directly: an add would infer one from
/// the working directory.
fn add_projectless(store: &Store<'_>, content: &str, id: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    Memories::new(store)
        .insert(&NewMemory::new(id, content, &now))
        .unwrap();
    id.to_string()
}

fn add(store: &Store<'_>, content: &str) -> String {
    add_in(store, content, "alpha", false)
}

fn found(store: &Store<'_>, id: &str) -> Vec<(String, DuplicateReason)> {
    let check = check_on_save(store, id);
    assert_eq!(check.duplicate_check_error, None);
    check
        .duplicate_candidates
        .into_iter()
        .map(|c| (c.memory.id, c.reason))
        .collect()
}

#[test]
fn an_exact_duplicate_is_returned() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = add(&store, "Releases go out on Tuesdays");
    let new = add(&store, "releases go out on  tuesdays");

    assert_eq!(found(&store, &new), vec![(old, DuplicateReason::Exact)]);
}

#[test]
fn a_reworded_restatement_is_returned_by_word_overlap() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = add(&store, "Nano prefers merge commits over squash merges");
    let new = add(
        &store,
        "Nano prefers merge commits over squash merges for PRs",
    );

    assert_eq!(found(&store, &new), vec![(old, DuplicateReason::Keyword)]);
}

#[test]
fn a_paraphrase_is_returned_by_embedding() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = add(&store, "The cat sleeps on the sofa");
    // One shared word, far below the keyword bar: found by its vector.
    let new = add(&store, "A feline naps upstairs on the sofa");
    for (id, text) in [
        (&old, "The cat sleeps on the sofa"),
        (&new, "A feline naps upstairs on the sofa"),
    ] {
        embed_and_store(&store, &TopicEmbedder, id, text).unwrap();
    }

    assert_eq!(found(&store, &new), vec![(old, DuplicateReason::Semantic)]);
}

#[test]
fn without_an_index_a_paraphrase_sharing_no_word_is_not_scanned_for() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = add(&store, "The cat sleeps on the sofa");
    let new = add(&store, "A feline naps upstairs");
    for (id, text) in [
        (&old, "The cat sleeps on the sofa"),
        (&new, "A feline naps upstairs"),
    ] {
        embed_and_store(&store, &TopicEmbedder, id, text).unwrap();
    }

    // The semantic side scores a bounded set of ids, never every chunk.
    assert!(found(&store, &new).is_empty());
}

#[test]
fn an_unrelated_memory_is_not_returned() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "Releases go out on Tuesdays");
    let new = add(&store, "The dashboard uses vendored JSX");

    assert!(found(&store, &new).is_empty());
}

#[test]
fn a_duplicate_in_another_project_is_not_returned() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add_in(&store, "Releases go out on Tuesdays", "beta", false);
    let new = add(&store, "Releases go out on Tuesdays");

    assert!(found(&store, &new).is_empty());
}

#[test]
fn a_projectless_duplicate_is_found_past_many_hits_in_other_projects() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let text = "Releases go out on Tuesdays";
    for i in 0..30 {
        add_in(&store, text, &format!("project-{i}"), false);
    }
    let old = add_projectless(&store, text, "old");
    let new = add_projectless(&store, text, "new");

    assert_eq!(found(&store, &new), vec![(old, DuplicateReason::Exact)]);
}

#[test]
fn a_sensitive_duplicate_is_not_returned() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add_in(&store, "Releases go out on Tuesdays", "alpha", true);
    let new = add(&store, "Releases go out on Tuesdays");

    assert!(found(&store, &new).is_empty());
}

#[test]
fn an_unknown_memory_finds_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "Releases go out on Tuesdays");

    assert!(found(&store, "mem_missing").is_empty());
}

#[test]
fn a_failed_check_is_reported_and_finds_nothing() {
    let check = duplicate_check("mem_x", Err(StoreError::Engine("boom".into())));

    assert!(check.duplicate_candidates.is_empty());
    assert_eq!(
        check.duplicate_check_error.as_deref(),
        Some("engine store: boom")
    );
}

#[test]
fn decompose_returns_duplicates_by_fact() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let captured = auto_capture(
        &store,
        &AutoCaptureInput {
            conversation: "user: when do releases go out\nassistant: Tuesdays".into(),
            summary: "release day".into(),
            title: String::new(),
            tags: vec![],
            category: "conversation".into(),
            metadata: serde_json::json!({}),
            project: None,
        },
    )
    .unwrap();
    let fact = |content: &str| AtomicFact {
        content: content.to_string(),
        memory_type: None,
        extra_tags: vec![],
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
        metadata: None,
    };
    let input = |content: &str| DecomposeInput {
        capture_id: captured.capture_id.clone(),
        facts: vec![fact(content)],
        capture_revision: None,
    };

    let first = decompose(&store, &input("Releases go out on Tuesdays"))
        .unwrap()
        .unwrap();
    assert!(first.duplicate_candidates.is_empty());

    let second = decompose(&store, &input("Releases go out on Tuesdays"))
        .unwrap()
        .unwrap();
    let dupes = &second.duplicate_candidates[&second.fact_ids[0]];
    assert_eq!(dupes[0].memory.id, first.fact_ids[0]);
    assert_eq!(dupes[0].reason, DuplicateReason::Exact);
}
