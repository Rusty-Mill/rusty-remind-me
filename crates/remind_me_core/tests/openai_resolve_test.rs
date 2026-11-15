//! The `openai` arm of embedder resolution, and the status it reports (#352).
//!
//! `openai_embedder_test.rs` covers `OpenAiEmbedder` built by hand. This file
//! covers what the environment builds: `resolve_embedder` with the model set,
//! unset or blank, and an address nothing answers at, plus the reason
//! `server_status` and `embedding_status` give for each.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::embedder::{
    available_embedder, configured_embedder, embedding_status, resolve_embedder,
    ConfiguredEmbedder, EMBEDDING_BACKEND_ENV, EMBEDDING_DIM_ENV, OPENAI_EMBED_MODEL_ENV,
    OPENAI_EMBED_URL_ENV,
};
use remind_me_core::status::{server_status, SubsystemStatus};
use remind_me_core::Database;
use std::net::TcpListener;
use std::sync::Mutex;

/// Held by every test here: they all set the process-global embedding vars.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets the `openai` backend at `url` with `model` (`None` leaves it unset).
fn set_openai(url: &str, model: Option<&str>) {
    test_env::set_var(EMBEDDING_BACKEND_ENV, "openai");
    test_env::set_var(EMBEDDING_DIM_ENV, "2");
    test_env::set_var(OPENAI_EMBED_URL_ENV, url);
    match model {
        Some(model) => test_env::set_var(OPENAI_EMBED_MODEL_ENV, model),
        None => test_env::remove_var(OPENAI_EMBED_MODEL_ENV),
    }
}

fn clear_openai() {
    for key in [
        EMBEDDING_BACKEND_ENV,
        EMBEDDING_DIM_ENV,
        OPENAI_EMBED_URL_ENV,
        OPENAI_EMBED_MODEL_ENV,
    ] {
        test_env::remove_var(key);
    }
}

/// An address nothing listens on: bound, then released.
fn dead_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port())
}

fn reason(status: &SubsystemStatus) -> &str {
    match status {
        SubsystemStatus::NotImplemented { reason } => reason,
        SubsystemStatus::Active => panic!("expected a reason, got Active"),
    }
}

#[test]
fn a_set_model_resolves_to_the_openai_embedder() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_openai("http://127.0.0.1:9", Some("qwen3-embedding-0.6b"));

    let identity = resolve_embedder().map(|e| e.identity());

    clear_openai();
    let identity = identity.expect("a set model resolves");
    assert_eq!(
        (
            identity.backend.as_str(),
            identity.model.as_str(),
            identity.dim
        ),
        ("openai", "qwen3-embedding-0.6b", 2)
    );
}

#[test]
fn an_unset_or_blank_model_resolves_to_none_and_names_the_model_setting() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for model in [None, Some(""), Some("  ")] {
        set_openai("http://127.0.0.1:9", model);
        let db = Database::open_in_memory().unwrap();

        let resolved = resolve_embedder().is_some();
        let configured = configured_embedder();
        let live = embedding_status();
        let report = server_status(&db.store()).unwrap();

        clear_openai();
        assert!(!resolved, "model {model:?} must not resolve");
        let ConfiguredEmbedder::Unusable { reason: why } = configured else {
            panic!("model {model:?} must be Unusable");
        };
        for text in [why.as_str(), reason(&live), reason(&report.embeddings)] {
            assert!(
                text.contains(OPENAI_EMBED_MODEL_ENV),
                "model {model:?}: {text}"
            );
            assert!(
                !text.contains("daemon") && !text.contains("ONNX"),
                "model {model:?} blamed the daemon or ONNX: {text}"
            );
        }
    }
}

#[test]
fn an_address_nothing_answers_at_resolves_but_is_not_available() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for url in [dead_url(), "not a url".to_string()] {
        set_openai(&url, Some("m"));

        let resolved = resolve_embedder().is_some();
        let available = available_embedder().is_some();
        let live = embedding_status();

        clear_openai();
        assert!(resolved, "{url}: the configuration is complete");
        assert!(!available, "{url}: nothing answers there");
        assert!(
            reason(&live).contains("not answering"),
            "{url}: {}",
            reason(&live)
        );
    }
}
