//! What every open reconciles before the store is used: the outbox gate
//! (`sync_flags.sync_enabled`) against this process's sync settings, and the
//! stored vectors against the configured embedding model (#96).
//!
//! The embedding checks plant a stale `embedding_meta` row plus a stored
//! vector through the repositories (no real Ollama daemon needed) and reopen
//! the same on-disk database to exercise the actual open-time wiring, not
//! just `vectors::reconcile_embedding_meta` in isolation.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::outbox::Outbox;
use remind_me_core::db::queries;
use remind_me_core::db::sync_state::SyncState;
use remind_me_core::db::vectors::Vectors;
use remind_me_core::db::Store;
use remind_me_core::embedder::{
    embedding_status, EMBEDDING_BACKEND_ENV, EMBEDDING_DIM_ENV, OLLAMA_MODEL_ENV,
    ONNX_MODEL_PATH_ENV, ONNX_TOKENIZER_PATH_ENV, OPENAI_EMBED_MODEL_ENV,
};
use remind_me_core::status::{server_status, SubsystemStatus};
use remind_me_core::sync::{HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::testing::{self, Table};
use remind_me_core::{Database, MemoryAddInput};
use std::path::PathBuf;
use std::sync::Mutex;

/// Every test here reads or writes process-global environment variables.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(
            format!(
                "rmm_open_{}_{}_{:?}",
                tag,
                std::process::id(),
                std::thread::current().id()
            )
            .replace(['(', ')', ' '], ""),
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("s.db"))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(p) = self.0.parent() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

fn add(store: &Store<'_>, content: &str) {
    queries::add_memory(
        store,
        MemoryAddInput {
            sensitive: false,
            content: content.into(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            ..Default::default()
        },
    )
    .unwrap();
}

/// The oldest queued outbox row's payload JSON. No send is recorded to any
/// remote in these tests, so every row is unsent to this one.
fn first_outbox_payload(store: &Store<'_>) -> String {
    Outbox::new(store)
        .unsent_to("any-remote", 0, 1)
        .unwrap()
        .remove(0)
        .payload_json
}

fn clear_sync_env() {
    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(HUB_URL_ENV);
    crate::test_env::remove_var(SYNC_SECRET_ENV);
}

#[test]
fn writes_reach_the_sync_outbox_when_sync_is_configured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(NODE_ID_ENV, "node-a");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");

    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "syncable");

    let payload = first_outbox_payload(&store);
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(parsed["content"], "syncable");
    assert!(parsed.get("base_weight").is_some());
    // A synced peer reconstructs a memory from the payload alone, so a column
    // missing here is data loss on the other node.
    for column in ["remind_at", "sensitive", "project", "written_by"] {
        assert!(
            parsed.get(column).is_some(),
            "the payload does not carry {column}"
        );
    }

    clear_sync_env();
}

#[test]
fn writes_do_not_reach_the_outbox_while_sync_is_unconfigured() {
    // The `#76` regression case: the outbox is gated on
    // sync_flags.sync_enabled, so a write on a node that has never
    // configured sync must not queue anything at all.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sync_env();

    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "not synced anywhere");

    assert_eq!(testing::count(&store, Table::SyncOutbox).unwrap(), 0);
    assert_eq!(
        SyncState::new(&store)
            .flag("sync_enabled")
            .unwrap()
            .as_deref(),
        Some("0"),
        "the gate is aligned with the configuration at open"
    );
}

#[test]
fn a_sent_outbox_row_past_the_retention_window_is_pruned_at_open() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("prune");
    crate::test_env::set_var(NODE_ID_ENV, "node-a");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");
    {
        let db = Database::open(&tmp.0).unwrap();
        let store = db.store();
        add(&store, "old");
        let rows = testing::outbox_rows(&store).unwrap();
        assert_eq!(rows.len(), 1);
        testing::set_outbox_column(
            &store,
            rows[0].id,
            "created_at",
            "2000-01-01T00:00:00+00:00",
        )
        .unwrap();
    }
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(testing::count(&db.store(), Table::SyncOutbox).unwrap(), 0);
    clear_sync_env();
}

// ---------------------------------------------------------------------------
// Embedding-model versioning at startup (#96)
// ---------------------------------------------------------------------------

fn plant_stale_vector(store: &Store<'_>, model: &str, dim: usize) {
    plant_vector(store, "ollama", model, dim);
}

/// A stored vector recorded as produced by `backend`/`model`/`dim`.
fn plant_vector(store: &Store<'_>, backend: &str, model: &str, dim: usize) {
    Memories::new(store)
        .insert(&NewMemory::new(
            "mem_versioning",
            "x",
            "2020-01-01T00:00:00Z",
        ))
        .unwrap();
    let vectors = Vectors::new(store);
    vectors
        .put("mem_versioning", 0, &vec![0u8; dim * 4])
        .unwrap();
    for (key, value) in [("backend", backend), ("model", model)] {
        vectors
            .set_meta(key, value, "2020-01-01T00:00:00Z")
            .unwrap();
    }
    vectors
        .set_meta("dim", &dim.to_string(), "2020-01-01T00:00:00Z")
        .unwrap();
}

fn stored_vector_counts(store: &Store<'_>) -> usize {
    Vectors::new(store).count().unwrap()
}

fn clear_embedding_env() {
    for key in [
        EMBEDDING_BACKEND_ENV,
        OLLAMA_MODEL_ENV,
        EMBEDDING_DIM_ENV,
        OPENAI_EMBED_MODEL_ENV,
        ONNX_MODEL_PATH_ENV,
        ONNX_TOKENIZER_PATH_ENV,
    ] {
        crate::test_env::remove_var(key);
    }
}

#[test]
fn reopening_with_an_unchanged_ollama_model_leaves_stored_vectors_alone() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_match");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "nomic-embed-text");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");

    {
        let db = Database::open(&tmp.0).unwrap();
        plant_stale_vector(&db.store(), "nomic-embed-text", 4);
    }

    // Reopening under the exact same configuration must not touch anything.
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(stored_vector_counts(&db.store()), 1);

    clear_embedding_env();
}

#[test]
fn reopening_with_a_changed_ollama_model_clears_stored_vectors() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_model_change");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "old-model");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");

    {
        let db = Database::open(&tmp.0).unwrap();
        plant_stale_vector(&db.store(), "old-model", 4);
    }

    crate::test_env::set_var(OLLAMA_MODEL_ENV, "new-model");
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(
        stored_vector_counts(&db.store()),
        0,
        "a changed REMIND_ME_OLLAMA_EMBED_MODEL must clear the stale vector on open"
    );

    clear_embedding_env();
}

#[test]
fn reopening_with_a_changed_embedding_dimension_clears_stored_vectors() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_dim_change");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "nomic-embed-text");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");

    {
        let db = Database::open(&tmp.0).unwrap();
        plant_stale_vector(&db.store(), "nomic-embed-text", 4);
    }

    crate::test_env::set_var(EMBEDDING_DIM_ENV, "8");
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(
        stored_vector_counts(&db.store()),
        0,
        "a changed REMIND_ME_EMBEDDING_DIM must clear the stale vector on open"
    );

    clear_embedding_env();
}

#[test]
fn a_first_ever_open_with_no_prior_embedding_meta_does_not_touch_vectors() {
    // No embedding_meta recorded yet: nothing to compare against, so a store
    // predating this feature must not spuriously clear a vector something
    // put there directly (as opposed to through embed_and_store, which
    // would have recorded its own identity already).
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_first_run");
    clear_embedding_env();

    {
        let db = Database::open(&tmp.0).unwrap();
        let store = db.store();
        Memories::new(&store)
            .insert(&NewMemory::new("mem_x", "x", "2020-01-01T00:00:00Z"))
            .unwrap();
        Vectors::new(&store).put("mem_x", 0, &[0u8; 16]).unwrap();
    }

    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "nomic-embed-text");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");
    let db = Database::open(&tmp.0).unwrap();
    let store = db.store();
    assert_eq!(stored_vector_counts(&store), 1);
    assert!(
        Vectors::new(&store).meta().unwrap().is_empty(),
        "the startup check itself must not write embedding_meta -- only a real embed does"
    );

    clear_embedding_env();
}

#[test]
fn reopening_with_the_embedding_backend_disabled_never_clears_stored_vectors() {
    // This crate's own adaptation (ADR-0002): embeddings are off unless
    // REMIND_ME_EMBEDDING_BACKEND=ollama is set. With it unset, nothing was
    // written by this process either, so the startup check is skipped
    // entirely rather than comparing against a meaningless "current"
    // identity.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_backend_disabled");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "nomic-embed-text");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");

    {
        let db = Database::open(&tmp.0).unwrap();
        plant_stale_vector(&db.store(), "nomic-embed-text", 4);
    }

    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(stored_vector_counts(&db.store()), 1);

    clear_embedding_env();
}

// ---------------------------------------------------------------------------
// An embedder that cannot embed never clears vectors (#351)
// ---------------------------------------------------------------------------

/// The reason a status reports, or a panic naming what it reported instead.
fn missing_reason(status: &SubsystemStatus) -> &str {
    match status {
        SubsystemStatus::NotImplemented { reason } => reason,
        other => panic!("expected the embedder reported unusable, got {other:?}"),
    }
}

/// Both status surfaces name `variable` as the reason embeddings are off.
fn assert_status_names(db: &Database, variable: &str) {
    let report = server_status(&db.store()).unwrap();
    let reason = missing_reason(&report.embeddings);
    assert!(reason.contains(variable), "server_status said {reason:?}");
    // `embedding_status` refuses before its network probe: nothing listens
    // at the default URL in a test, so a probe would also say "unavailable",
    // but for the wrong reason.
    let live = embedding_status();
    let reason = missing_reason(&live);
    assert!(
        reason.contains(variable),
        "embedding_status said {reason:?}"
    );
}

#[test]
fn reopening_openai_without_a_model_keeps_vectors_and_says_the_model_is_missing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Unset, empty and blank are the same slip: no model to embed with.
    for (case, model) in [("unset", None), ("empty", Some("")), ("blank", Some("  "))] {
        let tmp = TempDb::new(&format!("openai_no_model_{case}"));
        clear_embedding_env();
        crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "openai");
        crate::test_env::set_var(OPENAI_EMBED_MODEL_ENV, "m");
        crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");
        {
            let db = Database::open(&tmp.0).unwrap();
            plant_vector(&db.store(), "openai", "m", 4);
        }

        match model {
            None => crate::test_env::remove_var(OPENAI_EMBED_MODEL_ENV),
            Some(value) => crate::test_env::set_var(OPENAI_EMBED_MODEL_ENV, value),
        }
        let db = Database::open(&tmp.0).unwrap();
        assert_eq!(
            stored_vector_counts(&db.store()),
            1,
            "an {case} {OPENAI_EMBED_MODEL_ENV} must not clear the stored vectors"
        );
        assert_status_names(&db, OPENAI_EMBED_MODEL_ENV);
    }
    clear_embedding_env();
}

#[test]
fn reopening_onnx_without_paths_keeps_vectors_and_says_which_path_is_missing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (case, unset) in [
        ("model", ONNX_MODEL_PATH_ENV),
        ("tokenizer", ONNX_TOKENIZER_PATH_ENV),
    ] {
        let tmp = TempDb::new(&format!("onnx_no_{case}"));
        clear_embedding_env();
        crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "onnx");
        crate::test_env::set_var(ONNX_MODEL_PATH_ENV, "/models/e5.onnx");
        crate::test_env::set_var(ONNX_TOKENIZER_PATH_ENV, "/models/tokenizer.json");
        crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");
        {
            let db = Database::open(&tmp.0).unwrap();
            plant_vector(&db.store(), "onnx", "/models/e5.onnx", 4);
        }

        crate::test_env::remove_var(unset);
        let db = Database::open(&tmp.0).unwrap();
        assert_eq!(
            stored_vector_counts(&db.store()),
            1,
            "an unset {unset} must not clear the stored vectors"
        );
        assert_status_names(&db, unset);
    }
    clear_embedding_env();
}

#[test]
fn reopening_openai_with_a_different_model_still_clears_stored_vectors() {
    // Guard for the fix above: a real identity change still clears
    // (ADR-0002). A fix that skipped reconcile for every openai open, or
    // compared only the backend, would keep the stale vector and fail here.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("openai_model_change");
    clear_embedding_env();
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "openai");
    crate::test_env::set_var(OPENAI_EMBED_MODEL_ENV, "m");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");
    {
        let db = Database::open(&tmp.0).unwrap();
        plant_vector(&db.store(), "openai", "m", 4);
    }

    crate::test_env::set_var(OPENAI_EMBED_MODEL_ENV, "m2");
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(stored_vector_counts(&db.store()), 0);
    clear_embedding_env();
}
