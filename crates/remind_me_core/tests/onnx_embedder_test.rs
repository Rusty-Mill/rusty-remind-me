//! Coverage for `OnnxEmbedder` and the `onnx` backend dispatch in
//! `resolve_embedder`/`available_embedder`. Mirrors `reranker_test.rs`'s
//! split: everything checkable without a downloaded model runs
//! unconditionally; `with_the_feature`/`without_the_feature` cover the two
//! build configurations separately, using synthetic (not real, not
//! downloaded) files where a "the model failed to load" path needs
//! exercising — the same reasoning `reranker_test.rs` states for its own
//! `a_model_file_that_is_not_a_model_keeps_the_rrf_order`.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::embedder::{
    self, EmbedRole, Embedder, OnnxEmbedder, DEFAULT_EMBEDDING_DIM, EMBEDDING_BACKEND_ENV,
    EMBEDDING_DIM_ENV, ONNX_MODEL_PATH_ENV, ONNX_TOKENIZER_PATH_ENV,
};

/// The env vars this module reads are process-global; every test here holds
/// `ENV_LOCK`, the same convention `reranker_test.rs`/`ollama_embedder_test.rs`
/// establish for their own env-var-driven tests. A dedicated lock (not a
/// shared one) because this crate's other embedder-config tests
/// (`embedder.rs`'s own `#[cfg(test)]` module) do not touch `onnx`'s vars.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_env() {
    for var in [
        EMBEDDING_BACKEND_ENV,
        EMBEDDING_DIM_ENV,
        ONNX_MODEL_PATH_ENV,
        ONNX_TOKENIZER_PATH_ENV,
    ] {
        crate::test_env::remove_var(var);
    }
}

#[test]
fn resolve_embedder_dispatches_to_onnx_with_the_configured_paths_and_dim() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "onnx");
    crate::test_env::set_var(ONNX_MODEL_PATH_ENV, "/some/model.rten");
    crate::test_env::set_var(ONNX_TOKENIZER_PATH_ENV, "/some/tokenizer.json");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "384");

    let resolved = embedder::resolve_embedder().expect("onnx backend should resolve");
    let identity = resolved.identity();

    assert_eq!(identity.backend, "onnx");
    assert_eq!(identity.dim, 384);
    assert!(identity.model.contains("model.rten"), "{}", identity.model);

    clear_env();
}

#[test]
fn resolve_embedder_defaults_the_dimension_when_unset() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "onnx");
    crate::test_env::set_var(ONNX_MODEL_PATH_ENV, "/some/model.rten");
    crate::test_env::set_var(ONNX_TOKENIZER_PATH_ENV, "/some/tokenizer.json");

    let resolved = embedder::resolve_embedder().unwrap();
    assert_eq!(resolved.dim(), DEFAULT_EMBEDDING_DIM);

    clear_env();
}

#[test]
fn resolve_embedder_is_none_for_a_backend_that_is_neither_ollama_nor_onnx() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "something-else");
    assert!(embedder::resolve_embedder().is_none());
    clear_env();
}

#[test]
fn embed_of_an_empty_batch_returns_empty_without_touching_the_model() {
    // Reachable regardless of the `local-embed` feature: the empty-batch
    // short-circuit runs before `OnnxEmbedder` ever calls into the
    // feature-gated backend, so a path that does not exist is fine here.
    let embedder = OnnxEmbedder::new(
        "/nonexistent/model.rten",
        "/nonexistent/tokenizer.json",
        384,
    );
    let out = embedder.embed(&[], EmbedRole::Query).unwrap();
    assert!(out.is_empty());
}

// ---------------------------------------------------------------------------
// Feature off — the configuration most builds ship
// ---------------------------------------------------------------------------

#[cfg(not(feature = "local-embed"))]
mod without_the_feature {
    use super::*;

    #[test]
    fn embed_names_the_missing_feature_flag() {
        let embedder = OnnxEmbedder::new("/some/model.rten", "/some/tokenizer.json", 384);
        let err = embedder
            .embed(&["text".to_string()], EmbedRole::Query)
            .unwrap_err();
        assert!(
            err.0.contains("--features remind_me_core/local-embed"),
            "{}",
            err.0
        );
    }
}

// ---------------------------------------------------------------------------
// Feature on — everything checkable without a downloaded model
// ---------------------------------------------------------------------------

#[cfg(feature = "local-embed")]
mod with_the_feature {
    use super::*;

    #[test]
    fn a_model_file_that_is_not_a_model_is_a_clear_error_not_a_panic() {
        let dir = std::env::temp_dir().join("remind_me_onnx_bad_model_test");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.rten");
        let tokenizer = dir.join("tokenizer.json");
        std::fs::write(&model, b"not a model").unwrap();
        std::fs::write(&tokenizer, b"not a tokenizer").unwrap();

        let embedder = OnnxEmbedder::new(&model, &tokenizer, 384);
        let err = embedder
            .embed(&["text".to_string()], EmbedRole::Query)
            .unwrap_err();
        assert!(err.0.contains("could not load"), "{}", err.0);
        assert!(err.0.contains("model.rten"), "{}", err.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_model_path_is_a_clear_error_not_a_panic() {
        let embedder = OnnxEmbedder::new(
            "/definitely/does/not/exist/model.rten",
            "/definitely/does/not/exist/tokenizer.json",
            384,
        );
        let err = embedder
            .embed(&["text".to_string()], EmbedRole::Query)
            .unwrap_err();
        assert!(err.0.contains("could not load"), "{}", err.0);
    }
}
