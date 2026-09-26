//! Coverage for vector storage, brute-force semantic search, and
//! `remind_me_reindex`.
//!
//! A [`FakeEmbedder`] stands in for the real Ollama backend throughout: it
//! returns pre-defined vectors for known input strings, so a test can assert
//! an exact similarity ordering without a real embedding model or network
//! access. `OllamaEmbedder` itself — the HTTP client — has its own coverage
//! in `ollama_embedder_test.rs`, against a fake HTTP server.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::db::vectors::Vectors;
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::vectors::{
    delete_chunks_for_memory, dimension_of, embed_and_store, embedding_mismatch_info,
    fuse_query_embedding, mark_embedding_meta_current, reconcile_embedding_meta, reindex,
    reindex_with, semantic_search, semantic_search_scored,
};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::collections::HashMap;

/// Returns a pre-defined vector for each known input string, verbatim (no
/// chunking-awareness needed as long as test content stays under
/// `EMBED_CHUNK_CHARS`, so `chunk_text` never splits it).
struct FakeEmbedder {
    dim: usize,
    vectors: HashMap<String, Vec<f32>>,
}

impl FakeEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            dim,
            vectors: HashMap::new(),
        }
    }

    fn with(mut self, text: &str, vector: Vec<f32>) -> Self {
        assert_eq!(
            vector.len(),
            self.dim,
            "fixture vector length must match the embedder's dim"
        );
        self.vectors.insert(text.to_string(), vector);
        self
    }
}

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        texts
            .iter()
            .map(|t| {
                self.vectors
                    .get(t)
                    .cloned()
                    .ok_or_else(|| EmbedError(format!("FakeEmbedder has no vector for {:?}", t)))
            })
            .collect()
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "fake".to_string(),
            model: "fake-model".to_string(),
            dim: self.dim,
        }
    }
}

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
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
    .unwrap()
    .id
}

fn add_with_category(conn: &Connection, content: &str, category: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: category.to_string(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap()
    .id
}

fn chunk_count(conn: &Connection, memory_id: &str) -> usize {
    Vectors::new(conn).chunk_count(memory_id).unwrap()
}

// ---------------------------------------------------------------------------
// Vector round-trip and dimension inference
// ---------------------------------------------------------------------------

#[test]
fn embedding_a_memory_stores_one_chunk_and_dimension_infers_correctly() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokkas live on Rottnest Island");
    let embedder =
        FakeEmbedder::new(4).with("quokkas live on Rottnest Island", vec![1.0, 0.0, 0.0, 0.0]);

    let chunks = embed_and_store(&conn, &embedder, &id, "quokkas live on Rottnest Island").unwrap();

    assert_eq!(chunks, 1);
    assert_eq!(chunk_count(&conn, &id), 1);

    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT embedding FROM vec_chunks WHERE memory_id = ?",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dimension_of(&bytes),
        4,
        "384/768/1024 all round-trip the same way"
    );
    assert_eq!(bytes.len(), 16, "4 floats * 4 bytes");
}

#[test]
fn re_embedding_replaces_rather_than_accumulates_chunks() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "original text");
    let embedder = FakeEmbedder::new(2)
        .with("original text", vec![1.0, 0.0])
        .with("revised text", vec![0.0, 1.0]);

    embed_and_store(&conn, &embedder, &id, "original text").unwrap();
    assert_eq!(chunk_count(&conn, &id), 1);

    embed_and_store(&conn, &embedder, &id, "revised text").unwrap();

    assert_eq!(
        chunk_count(&conn, &id),
        1,
        "not 2 -- the old chunk was replaced"
    );
}

#[test]
fn embedding_blank_content_stores_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "placeholder");
    let embedder = FakeEmbedder::new(2);

    let chunks = embed_and_store(&conn, &embedder, &id, "   ").unwrap();

    assert_eq!(chunks, 0);
    assert_eq!(chunk_count(&conn, &id), 0);
}

#[test]
fn embedding_an_unknown_memory_id_is_a_silent_no_op() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let embedder = FakeEmbedder::new(2).with("text", vec![1.0, 0.0]);

    let chunks = embed_and_store(&conn, &embedder, "mem_ghost", "text").unwrap();

    assert_eq!(chunks, 0);
}

// ---------------------------------------------------------------------------
// Deletion cleans up chunks
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_memory_removes_its_chunks_and_embeddings() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "doomed content");
    let embedder = FakeEmbedder::new(2).with("doomed content", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &id, "doomed content").unwrap();
    assert_eq!(chunk_count(&conn, &id), 1);

    queries::delete_memory(&conn, &id).unwrap();

    assert_eq!(Vectors::new(&conn).count().unwrap(), 0);
}

#[test]
fn a_deleted_memorys_embeddings_do_not_leak_to_its_successor() {
    // Chunks were once keyed on rowid, which SQLite reuses; they are keyed on
    // the memory id now, and a memory created after a deletion must still
    // start with nothing.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let first = add(&conn, "first memory");
    let embedder = FakeEmbedder::new(2)
        .with("first memory", vec![1.0, 0.0])
        .with("second memory", vec![0.0, 1.0]);
    embed_and_store(&conn, &embedder, &first, "first memory").unwrap();
    queries::delete_memory(&conn, &first).unwrap();

    let second = add(&conn, "second memory");
    // The new memory is not embedded yet -- confirm it inherited nothing.
    assert_eq!(chunk_count(&conn, &second), 0);

    embed_and_store(&conn, &embedder, &second, "second memory").unwrap();
    assert_eq!(chunk_count(&conn, &second), 1);
}

// ---------------------------------------------------------------------------
// Direct delete_chunks_for_memory
// ---------------------------------------------------------------------------

#[test]
fn delete_chunks_for_memory_is_a_no_op_on_a_never_embedded_memory() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "never embedded");
    let removed = delete_chunks_for_memory(&conn, &id).unwrap();

    assert_eq!(removed, 0);
}

// ---------------------------------------------------------------------------
// Semantic search
// ---------------------------------------------------------------------------

#[test]
fn semantic_search_ranks_by_similarity_to_the_query() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let close = add(&conn, "quokkas are marsupials found on Rottnest Island");
    let far = add(&conn, "the deploy window is Tuesdays at 2pm");

    let embedder = FakeEmbedder::new(3)
        .with(
            "quokkas are marsupials found on Rottnest Island",
            vec![1.0, 0.0, 0.0],
        )
        .with("the deploy window is Tuesdays at 2pm", vec![0.0, 1.0, 0.0])
        .with("tell me about quokkas", vec![0.9, 0.1, 0.0]);
    embed_and_store(
        &conn,
        &embedder,
        &close,
        "quokkas are marsupials found on Rottnest Island",
    )
    .unwrap();
    embed_and_store(
        &conn,
        &embedder,
        &far,
        "the deploy window is Tuesdays at 2pm",
    )
    .unwrap();

    let results = semantic_search(&conn, &embedder, "tell me about quokkas", 10, None).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, close, "the near-parallel vector ranks first");
    assert_eq!(results[1].id, far);
}

#[test]
fn semantic_search_keeps_only_a_memorys_single_best_chunk() {
    // A memory that happens to own several chunks must not out-rank one
    // with a single chunk purely for having more shots at matching.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let multi = add(&conn, "multi chunk memory");
    let single = add(&conn, "single chunk memory");
    let embedder = FakeEmbedder::new(2)
        .with("multi chunk memory", vec![0.1, 0.0])
        .with("single chunk memory", vec![0.9, 0.0])
        .with("query text", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &multi, "multi chunk memory").unwrap();
    embed_and_store(&conn, &embedder, &single, "single chunk memory").unwrap();

    let results = semantic_search(&conn, &embedder, "query text", 10, None).unwrap();

    assert_eq!(results[0].id, single, "0.9 similarity beats 0.1");
}

#[test]
fn semantic_search_excludes_superseded_and_deleted_memories() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let stale = add(&conn, "stale content");
    let embedder = FakeEmbedder::new(2)
        .with("stale content", vec![1.0, 0.0])
        .with("removed content", vec![1.0, 0.0])
        .with("query", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &stale, "stale content").unwrap();
    conn.execute(
        "UPDATE memories SET superseded_by = 'mem_new' WHERE id = ?",
        [&stale],
    )
    .unwrap();

    let removed = add(&conn, "removed content");
    embed_and_store(&conn, &embedder, &removed, "removed content").unwrap();
    conn.execute(
        "UPDATE memories SET deleted_at = '2026-01-01T00:00:00Z' WHERE id = ?",
        [&removed],
    )
    .unwrap();

    let results = semantic_search(&conn, &embedder, "query", 10, None).unwrap();
    assert!(results.is_empty());
}

#[test]
fn semantic_search_filters_by_category() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let wildlife = add_with_category(&conn, "quokka content", "wildlife");
    let general = add_with_category(&conn, "general content", "general");
    let embedder = FakeEmbedder::new(2)
        .with("quokka content", vec![1.0, 0.0])
        .with("general content", vec![1.0, 0.0])
        .with("query", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &wildlife, "quokka content").unwrap();
    embed_and_store(&conn, &embedder, &general, "general content").unwrap();

    let results = semantic_search(&conn, &embedder, "query", 10, Some("wildlife")).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, wildlife);
}

#[test]
fn semantic_search_over_an_empty_store_returns_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let embedder = FakeEmbedder::new(2).with("query", vec![1.0, 0.0]);

    let results = semantic_search(&conn, &embedder, "query", 10, None).unwrap();

    assert!(results.is_empty());
}

#[test]
fn a_stale_dimension_vector_is_skipped_rather_than_crashing_the_scan() {
    // As if REMIND_ME_EMBEDDING_DIM changed and old vectors were never
    // reindexed: a leftover vector at the wrong width must not panic the
    // dot product, and must not be treated as a match either.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "old width content");
    // 3 floats (12 bytes) where the query embedder produces 2.
    Vectors::new(&conn).put(&id, 0, &[0u8; 12]).unwrap();

    let embedder = FakeEmbedder::new(2).with("query", vec![1.0, 0.0]);
    let results = semantic_search(&conn, &embedder, "query", 10, None).unwrap();

    assert!(
        results.is_empty(),
        "the mismatched-width vector is skipped, not matched"
    );
}

// ---------------------------------------------------------------------------
// Reindex
// ---------------------------------------------------------------------------

#[test]
fn reindex_reports_degraded_with_no_embedder_configured() {
    // Deliberately does not touch REMIND_ME_EMBEDDING_BACKEND: this asserts
    // the default (unset) case, which every other test in this crate's
    // suite already relies on being the ambient state.
    crate::test_env::remove_var(remind_me_core::embedder::EMBEDDING_BACKEND_ENV);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "never embedded");

    let result = reindex(&conn).unwrap();

    assert!(result.degraded);
    assert_eq!(result.embedded, 0);
}

#[test]
fn reindex_with_embeds_only_memories_missing_a_vector() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let already_embedded = add(&conn, "already embedded");
    let missing = add(&conn, "missing content");
    let embedder = FakeEmbedder::new(2)
        .with("already embedded", vec![1.0, 0.0])
        .with("missing content", vec![0.0, 1.0]);
    embed_and_store(&conn, &embedder, &already_embedded, "already embedded").unwrap();

    let result = reindex_with(&conn, &embedder).unwrap();

    assert!(!result.degraded);
    assert_eq!(
        result.missing, 1,
        "only the never-embedded memory counts as missing"
    );
    assert_eq!(result.embedded, 1);
    assert_eq!(result.chunks_created, 1);
    assert_eq!(chunk_count(&conn, &missing), 1);
}

#[test]
fn reindex_with_is_idempotent_and_preserves_existing_embeddings() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "some content");
    let embedder = FakeEmbedder::new(2).with("some content", vec![1.0, 0.0]);

    let first = reindex_with(&conn, &embedder).unwrap();
    assert_eq!(first.embedded, 1);

    let second = reindex_with(&conn, &embedder).unwrap();

    assert_eq!(
        second.missing, 0,
        "nothing left unembedded on the second pass"
    );
    assert_eq!(second.embedded, 0);
    assert_eq!(second.chunks_created, 0);
    assert_eq!(
        chunk_count(&conn, &id),
        1,
        "the original chunk was left alone, not duplicated"
    );
}

#[test]
fn reindex_with_over_an_empty_store_does_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let embedder = FakeEmbedder::new(2);

    let result = reindex_with(&conn, &embedder).unwrap();

    assert!(!result.degraded);
    assert_eq!(result.missing, 0);
    assert_eq!(result.embedded, 0);
    assert_eq!(result.chunks_created, 0);
}

// ---------------------------------------------------------------------------
// Embedding-model versioning (#96)
// ---------------------------------------------------------------------------

fn identity(backend: &str, model: &str, dim: usize) -> EmbeddingIdentity {
    EmbeddingIdentity {
        backend: backend.to_string(),
        model: model.to_string(),
        dim,
    }
}

#[test]
fn a_fresh_store_with_no_recorded_meta_reports_no_mismatch() {
    // The "first-ever run" case: nothing has been recorded yet, so there is
    // no old model to have changed away from.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let info =
        embedding_mismatch_info(&conn, &identity("ollama", "nomic-embed-text", 384)).unwrap();

    assert!(info.is_none());
}

#[test]
fn reconciling_a_fresh_store_does_not_clear_anything() {
    // Same case, but through the clearing entry point: a first-ever run must
    // not spuriously wipe vectors that were only just written under the
    // current config.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokkas live on Rottnest Island");
    let embedder = FakeEmbedder::new(2).with("quokkas live on Rottnest Island", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &id, "quokkas live on Rottnest Island").unwrap();

    let cleared = reconcile_embedding_meta(&conn, &embedder.identity()).unwrap();

    assert!(cleared.is_none());
    assert_eq!(chunk_count(&conn, &id), 1, "the fresh vector must survive");
}

#[test]
fn the_same_model_across_runs_is_a_no_op() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "same model content");
    let embedder = FakeEmbedder::new(2).with("same model content", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &id, "same model content").unwrap();

    // embed_and_store already recorded the fake embedder's identity; asking
    // again with the exact same identity must report no mismatch.
    let info = embedding_mismatch_info(&conn, &embedder.identity()).unwrap();
    assert!(info.is_none());

    let cleared = reconcile_embedding_meta(&conn, &embedder.identity()).unwrap();
    assert!(cleared.is_none());
    assert_eq!(chunk_count(&conn, &id), 1, "nothing was cleared");
}

#[test]
fn a_changed_model_name_is_detected() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    mark_embedding_meta_current(&conn, &identity("ollama", "old-model", 384)).unwrap();

    let info = embedding_mismatch_info(&conn, &identity("ollama", "new-model", 384))
        .unwrap()
        .expect("a different model name must be reported as a mismatch");

    assert_eq!(info.stored.model, "old-model");
    assert_eq!(info.current.model, "new-model");
}

#[test]
fn a_changed_dimension_is_detected() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    mark_embedding_meta_current(&conn, &identity("ollama", "nomic-embed-text", 384)).unwrap();

    let info = embedding_mismatch_info(&conn, &identity("ollama", "nomic-embed-text", 768))
        .unwrap()
        .expect("a different dimension must be reported as a mismatch");

    assert_eq!(info.stored.dim, 384);
    assert_eq!(info.current.dim, 768);
}

#[test]
fn a_changed_backend_is_detected() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    mark_embedding_meta_current(&conn, &identity("ollama", "nomic-embed-text", 384)).unwrap();

    let info = embedding_mismatch_info(&conn, &identity("onnx", "nomic-embed-text", 384))
        .unwrap()
        .expect("a different backend must be reported as a mismatch");

    assert_eq!(info.stored.backend, "ollama");
    assert_eq!(info.current.backend, "onnx");
}

#[test]
fn a_detected_mismatch_clears_every_stored_vector_and_chunk() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "stale model content");
    let old_embedder = FakeEmbedder::new(2).with("stale model content", vec![1.0, 0.0]);
    embed_and_store(&conn, &old_embedder, &id, "stale model content").unwrap();
    assert_eq!(chunk_count(&conn, &id), 1);

    let new_identity = identity("ollama", "a-different-model", 2);
    let cleared = reconcile_embedding_meta(&conn, &new_identity)
        .unwrap()
        .expect("the fake embedder's identity no longer matches new_identity");

    assert_eq!(cleared.stored.model, "fake-model");
    assert_eq!(cleared.current.model, "a-different-model");
    assert_eq!(chunk_count(&conn, &id), 0, "vec_chunks must be cleared");
    assert_eq!(Vectors::new(&conn).count().unwrap(), 0);
}

#[test]
fn reconcile_leaves_the_meta_record_stale_until_a_real_reembed() {
    // Deliberately not updated by reconcile itself -- only a real
    // (re-)embed clears the flag, so the mismatch stays visible across
    // every open/connection until that happens, matching the reference.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    mark_embedding_meta_current(&conn, &identity("ollama", "old-model", 384)).unwrap();

    let current = identity("ollama", "new-model", 384);
    reconcile_embedding_meta(&conn, &current).unwrap();

    let info = embedding_mismatch_info(&conn, &current).unwrap();
    assert!(
        info.is_some(),
        "the mismatch must still be flagged after reconciling"
    );
    assert_eq!(info.unwrap().stored.model, "old-model");
}

#[test]
fn a_real_reembed_after_a_mismatch_clears_the_flag() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content to re-embed");
    mark_embedding_meta_current(&conn, &identity("ollama", "old-model", 2)).unwrap();

    let embedder = FakeEmbedder::new(2).with("content to re-embed", vec![1.0, 0.0]);
    reconcile_embedding_meta(&conn, &embedder.identity()).unwrap();
    assert!(embedding_mismatch_info(&conn, &embedder.identity())
        .unwrap()
        .is_some());

    embed_and_store(&conn, &embedder, &id, "content to re-embed").unwrap();

    assert!(
        embedding_mismatch_info(&conn, &embedder.identity())
            .unwrap()
            .is_none(),
        "re-embedding under the fake embedder's own identity clears the mismatch"
    );
}

#[test]
fn embedding_a_memory_records_the_embedders_identity_in_embedding_meta() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokkas");
    let embedder = FakeEmbedder::new(3).with("quokkas", vec![1.0, 0.0, 0.0]);

    embed_and_store(&conn, &embedder, &id, "quokkas").unwrap();

    let recorded: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT key, value FROM embedding_meta ORDER BY key")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(
        recorded,
        vec![
            ("backend".to_string(), "fake".to_string()),
            ("dim".to_string(), "3".to_string()),
            ("model".to_string(), "fake-model".to_string()),
        ]
    );
}

#[test]
fn embedding_blank_content_does_not_record_embedding_meta() {
    // No chunks were actually stored, so there is nothing to claim
    // responsibility for.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "placeholder");
    let embedder = FakeEmbedder::new(2);

    embed_and_store(&conn, &embedder, &id, "   ").unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM embedding_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

// ---------------------------------------------------------------------------
// fuse_query_embedding / HyDE-style expansion
// ---------------------------------------------------------------------------

/// Records which [`EmbedRole`] each input text was embedded with, keyed by
/// text — used only to assert `fuse_query_embedding`'s role assignment
/// (query vs. passage), which `FakeEmbedder` above deliberately ignores.
struct RoleTrackingEmbedder {
    dim: usize,
    vectors: HashMap<String, Vec<f32>>,
    roles_seen: std::sync::Mutex<HashMap<String, EmbedRole>>,
}

impl RoleTrackingEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            dim,
            vectors: HashMap::new(),
            roles_seen: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn with(mut self, text: &str, vector: Vec<f32>) -> Self {
        assert_eq!(vector.len(), self.dim);
        self.vectors.insert(text.to_string(), vector);
        self
    }

    fn role_of(&self, text: &str) -> Option<EmbedRole> {
        self.roles_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(text)
            .copied()
    }
}

impl Embedder for RoleTrackingEmbedder {
    fn embed(&self, texts: &[String], role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut seen = self.roles_seen.lock().unwrap_or_else(|e| e.into_inner());
        texts
            .iter()
            .map(|t| {
                seen.insert(t.clone(), role);
                self.vectors
                    .get(t)
                    .cloned()
                    .ok_or_else(|| EmbedError(format!("no vector for {:?}", t)))
            })
            .collect()
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "fake".to_string(),
            model: "role-tracking".to_string(),
            dim: self.dim,
        }
    }
}

#[test]
fn fuse_query_embedding_of_a_single_text_is_that_texts_own_normalized_vector() {
    let embedder = FakeEmbedder::new(2).with("hello", vec![3.0, 4.0]);

    let fused = fuse_query_embedding(&embedder, &["hello".to_string()]).unwrap();

    assert!((fused[0] - 0.6).abs() < 1e-6);
    assert!((fused[1] - 0.8).abs() < 1e-6);
}

#[test]
fn fuse_query_embedding_averages_and_renormalizes_several_texts() {
    let embedder = FakeEmbedder::new(2)
        .with("query text", vec![1.0, 0.0])
        .with("hyde passage", vec![0.0, 1.0]);

    let fused = fuse_query_embedding(
        &embedder,
        &["query text".to_string(), "hyde passage".to_string()],
    )
    .unwrap();

    // Mean of [1,0] and [0,1] is [0.5,0.5]; L2-normalised is [1/sqrt(2), 1/sqrt(2)].
    let expected = 1.0 / std::f32::consts::SQRT_2;
    assert!((fused[0] - expected).abs() < 1e-6);
    assert!((fused[1] - expected).abs() < 1e-6);
}

#[test]
fn fuse_query_embedding_of_no_texts_is_empty() {
    let embedder = FakeEmbedder::new(2);
    assert!(fuse_query_embedding(&embedder, &[]).unwrap().is_empty());
}

#[test]
fn fuse_query_embedding_sends_the_query_role_to_the_first_text_and_passage_role_to_the_rest() {
    let embedder = RoleTrackingEmbedder::new(2)
        .with("the query", vec![1.0, 0.0])
        .with("expansion one", vec![0.0, 1.0])
        .with("expansion two", vec![1.0, 1.0]);

    fuse_query_embedding(
        &embedder,
        &[
            "the query".to_string(),
            "expansion one".to_string(),
            "expansion two".to_string(),
        ],
    )
    .unwrap();

    assert_eq!(embedder.role_of("the query"), Some(EmbedRole::Query));
    assert_eq!(embedder.role_of("expansion one"), Some(EmbedRole::Passage));
    assert_eq!(embedder.role_of("expansion two"), Some(EmbedRole::Passage));
}

#[test]
fn semantic_search_scored_with_extra_texts_can_change_the_winner_vs_the_plain_query() {
    // The plain query alone is closest to `off_topic`; a HyDE-style
    // expansion text pulls the fused vector toward `on_topic` instead —
    // proving `extra_texts` is actually wired into the search vector, not
    // silently ignored.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let off_topic = add(&conn, "off topic memory");
    let on_topic = add(&conn, "on topic memory");
    let embedder = FakeEmbedder::new(2)
        .with("query", vec![1.0, 0.05])
        .with("expansion", vec![0.0, 1.0])
        .with("off topic memory", vec![1.0, 0.0])
        .with("on topic memory", vec![0.0, 1.0]);
    embed_and_store(&conn, &embedder, &off_topic, "off topic memory").unwrap();
    embed_and_store(&conn, &embedder, &on_topic, "on topic memory").unwrap();

    let plain = semantic_search_scored(&conn, &embedder, "query", &[], 10, None).unwrap();
    let expanded = semantic_search_scored(
        &conn,
        &embedder,
        "query",
        &["expansion".to_string()],
        10,
        None,
    )
    .unwrap();

    assert_eq!(
        plain[0].0.id, off_topic,
        "plain query alone favors off_topic"
    );
    assert_eq!(
        expanded[0].0.id, on_topic,
        "the expansion text should pull the fused vector toward on_topic"
    );
}
