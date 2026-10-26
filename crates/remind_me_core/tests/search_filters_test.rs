//! `remind_me_search`'s tag, entity and repo filters: each alone, combined,
//! and on both halves of the hybrid search.

use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::entity::{link_memory_entity, upsert_entity};
use remind_me_core::vectors::embed_and_store;
use remind_me_core::{Database, EntityInput, MemorySearchInput, SearchFilters, TagMatch};

const NOW: &str = "2026-10-26T00:00:00+00:00";

/// Insert a row carrying the columns these filters read.
fn put(store: &Store<'_>, id: &str, content: &str, tags: &[&str], remote: Option<&str>) {
    Memories::new(store)
        .insert(&NewMemory {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            git_remote: remote.map(str::to_string),
            ..NewMemory::new(id, content, NOW)
        })
        .unwrap();
}

/// Four "mux" notes across two repos, one linked to `rusty_mux` and one
/// naming it only as its subject.
fn fixture(store: &Store<'_>) {
    put(
        store,
        "a",
        "mux pane layout",
        &["mux", "ui"],
        Some("github.com/Rusty-Mill/rusty_mux"),
    );
    put(
        store,
        "b",
        "mux session restore",
        &["mux"],
        Some("github.com/Rusty-Mill/rusty_mux"),
    );
    put(
        store,
        "c",
        "mux in the terminal",
        &["ui"],
        Some("github.com/Rusty-Mill/rusty_term"),
    );
    Memories::new(store)
        .insert(&NewMemory {
            subject: Some("Rusty_Mux".into()),
            ..NewMemory::new("d", "mux keybindings", NOW)
        })
        .unwrap();
    let entity = upsert_entity(
        store,
        &EntityInput {
            name: "rusty_mux".into(),
            kind: Some("project".into()),
            aliases: vec!["the multiplexer".into()],
        },
    )
    .unwrap();
    link_memory_entity(store, "a", &entity.id).unwrap();
}

fn input(query: &str, tags: &[&str], filters: SearchFilters) -> MemorySearchInput {
    MemorySearchInput {
        query: query.into(),
        tags: (!tags.is_empty()).then(|| tags.iter().map(|t| t.to_string()).collect()),
        filters,
        // The fixture is dated; these tests are about filters, not decay.
        include_dormant: true,
        token_budget: 100_000,
        ..Default::default()
    }
}

fn ids(store: &Store<'_>, input: &MemorySearchInput) -> Vec<String> {
    let mut got: Vec<String> = queries::search_memories_with_embedder(store, input, None)
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect();
    got.sort();
    got
}

fn search(query: &str, tags: &[&str], filters: SearchFilters) -> Vec<String> {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    fixture(&store);
    ids(&store, &input(query, tags, filters))
}

fn entity(name: &str) -> SearchFilters {
    SearchFilters {
        entity: Some(name.into()),
        ..Default::default()
    }
}

fn repo(url: &str) -> SearchFilters {
    SearchFilters {
        repo: Some(url.into()),
        ..Default::default()
    }
}

#[test]
fn no_filters_match_everything() {
    assert_eq!(
        search("mux", &[], SearchFilters::default()),
        ["a", "b", "c", "d"]
    );
}

#[test]
fn tags_default_to_all_of() {
    assert_eq!(
        search("mux", &["mux"], SearchFilters::default()),
        ["a", "b"]
    );
    assert_eq!(
        search("mux", &["mux", "ui"], SearchFilters::default()),
        ["a"]
    );
}

#[test]
fn tags_any_of_takes_either() {
    let any = SearchFilters {
        tag_mode: TagMatch::Any,
        ..Default::default()
    };
    assert_eq!(search("mux", &["mux", "ui"], any), ["a", "b", "c"]);
}

#[test]
fn blank_tags_are_ignored() {
    assert_eq!(
        search("mux", &[" ", ""], SearchFilters::default()),
        ["a", "b", "c", "d"]
    );
}

#[test]
fn entity_matches_links_and_subject() {
    assert_eq!(search("mux", &[], entity("rusty_mux")), ["a", "d"]);
}

#[test]
fn entity_resolves_aliases_and_case() {
    assert_eq!(search("mux", &[], entity("The Multiplexer")), ["a", "d"]);
    assert_eq!(search("mux", &[], entity("  RUSTY_MUX ")), ["a", "d"]);
}

#[test]
fn unknown_entity_is_an_empty_result_not_an_error() {
    assert!(search("mux", &[], entity("nobody")).is_empty());
}

#[test]
fn repo_matches_any_remote_form() {
    for url in [
        "github.com/Rusty-Mill/rusty_mux",
        "https://github.com/Rusty-Mill/rusty_mux.git",
        "git@github.com:Rusty-Mill/rusty_mux.git",
        "https://GitHub.com/rusty-mill/RUSTY_MUX/",
    ] {
        assert_eq!(search("mux", &[], repo(url)), ["a", "b"], "{url}");
    }
}

#[test]
fn repo_excludes_rows_without_a_remote() {
    assert_eq!(
        search("mux", &[], repo("github.com/Rusty-Mill/rusty_term")),
        ["c"]
    );
    assert!(search("mux", &[], repo("github.com/acme/none")).is_empty());
}

#[test]
fn filters_combine_with_each_other_and_the_query() {
    let both = SearchFilters {
        entity: Some("rusty_mux".into()),
        repo: Some("github.com/Rusty-Mill/rusty_mux".into()),
        ..Default::default()
    };
    assert_eq!(search("mux", &["ui"], both), ["a"]);
    assert_eq!(
        search("session", &[], repo("github.com/Rusty-Mill/rusty_mux")),
        ["b"]
    );
    assert!(search("terminal", &["mux"], SearchFilters::default()).is_empty());
}

#[test]
fn filters_apply_before_the_limit() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for i in 0..30 {
        put(&store, &format!("x{i:02}"), "mux mux", &[], None);
    }
    fixture(&store);
    let mut unfiltered = input("mux", &[], SearchFilters::default());
    unfiltered.limit = 2;
    assert!(ids(&store, &unfiltered)
        .iter()
        .all(|id| id.starts_with('x')));
    let mut narrowed = input("mux", &[], repo("github.com/Rusty-Mill/rusty_mux"));
    narrowed.limit = 2;
    assert_eq!(ids(&store, &narrowed), ["a", "b"]);
}

#[test]
fn filters_deserialise_from_flat_tool_arguments() {
    let input: MemorySearchInput = serde_json::from_value(serde_json::json!({
        "query": "mux",
        "tags": ["a", "b"],
        "tag_mode": "any",
        "entity": "rusty_mux",
        "repo": "github.com/o/r"
    }))
    .unwrap();
    assert_eq!(input.filters.tag_mode, TagMatch::Any);
    assert_eq!(input.filters.entity.as_deref(), Some("rusty_mux"));
    assert_eq!(input.filters.repo.as_deref(), Some("github.com/o/r"));
    assert!(serde_json::from_value::<MemorySearchInput>(
        serde_json::json!({ "query": "mux", "tag_mode": "some" })
    )
    .is_err());
}

/// Every text embeds to the same point, so the semantic half returns every
/// row with vectors, whatever the keyword half matched.
struct Constant;

impl Embedder for Constant {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }

    fn dim(&self) -> usize {
        2
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "constant".into(),
            model: "test".into(),
            dim: 2,
        }
    }
}

#[test]
fn filters_apply_to_semantic_hits_too() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    put(&store, "kept", "pane layout", &["mux"], None);
    put(&store, "dropped", "pane layout", &["term"], None);
    for (id, content) in [("kept", "pane layout"), ("dropped", "pane layout")] {
        embed_and_store(&store, &Constant, id, content).unwrap();
    }

    let unfiltered = input("multiplexer", &[], SearchFilters::default());
    let all: Vec<String> =
        queries::search_memories_with_embedder(&store, &unfiltered, Some(&Constant))
            .unwrap()
            .into_iter()
            .map(|r| r.memory.id)
            .collect();
    assert!(
        all.contains(&"dropped".to_string()),
        "semantic half reaches it: {all:?}"
    );

    let narrowed = input("multiplexer", &["mux"], SearchFilters::default());
    let got: Vec<String> =
        queries::search_memories_with_embedder(&store, &narrowed, Some(&Constant))
            .unwrap()
            .into_iter()
            .map(|r| r.memory.id)
            .collect();
    assert_eq!(got, ["kept"]);
}

/// Texts naming "term" embed to one point and everything else nearby, so
/// "term" rows always outrank the rest semantically.
struct TermFirst;

impl Embedder for TermFirst {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                if t.contains("term") {
                    vec![1.0, 0.0]
                } else {
                    vec![0.6, 0.8]
                }
            })
            .collect())
    }

    fn dim(&self) -> usize {
        2
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "term-first".into(),
            model: "test".into(),
            dim: 2,
        }
    }
}

#[test]
fn semantic_filters_refill_past_out_of_scope_neighbours() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Five better semantic matches outside the filter, more than the
    // semantic pool (2 x limit) holds, and one worse match inside it.
    for i in 0..5 {
        let id = format!("t{i}");
        put(&store, &id, "term note", &["term"], None);
        embed_and_store(&store, &TermFirst, &id, "term note").unwrap();
    }
    put(&store, "kept", "pane layout", &["mux"], None);
    embed_and_store(&store, &TermFirst, "kept", "pane layout").unwrap();

    let mut narrowed = input("term", &["mux"], SearchFilters::default());
    narrowed.limit = 1;
    let got: Vec<String> =
        queries::search_memories_with_embedder(&store, &narrowed, Some(&TermFirst))
            .unwrap()
            .into_iter()
            .map(|r| r.memory.id)
            .collect();
    assert_eq!(got, ["kept"]);
}
