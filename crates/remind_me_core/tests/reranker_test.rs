//! Cross-encoder reranking (gap E8, issue #155, part 2 of 2).
//!
//! Unlike OCR and transcription, almost all of this **is** testable without a
//! model, because the reference's own design injects the scorer. What a
//! cross-encoder returns for a real pair needs real weights; what the pipeline
//! does with those numbers — which candidates it touches, which it leaves
//! alone, what it does when the scorer misbehaves — does not. That second part
//! is where a reranker silently corrupts a result page, so it is asserted
//! unconditionally and in detail.
//!
//! The governing rule under test throughout: **reranking may never break
//! search.** Every failure path returns the incoming order untouched.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::models::MemorySearchResult;
use remind_me_core::reranker::{self, Status};

/// The reranker reads process-wide env vars, so these run one at a time.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_env() {
    for var in [
        reranker::BACKEND_ENV,
        reranker::TOP_K_ENV,
        reranker::MODEL_ENV,
        reranker::TOKENIZER_ENV,
    ] {
        crate::test_env::remove_var(var);
    }
}

fn result(id: &str, content: &str, score: f64) -> MemorySearchResult {
    MemorySearchResult {
        memory: remind_me_core::Memory {
            id: id.to_string(),
            content: content.to_string(),
            category: "general".to_string(),
            tags: vec![],
            source: "manual".to_string(),
            metadata: serde_json::json!({}),
            created_at: String::new(),
            updated_at: String::new(),
            capture_id: None,
            subject: None,
            predicate: None,
            object: None,
            superseded_by: None,
            decay_rate: 0.0,
            vitality: 0.0,
            base_weight: 0.0,
            access_count: 0,
            accessed_at: String::new(),
            doc_id: None,
            chunk_index: None,
            remind_at: None,
            sensitive: false,
            // Present so a Memory can round-trip to JSON (#198); reranking
            // reads none of them.
            memory_type: None,
            status: None,
            node_id: None,
            client: None,
            source_capture_id: None,
            deleted_at: None,
        },
        score,
        fts_score: Some(score),
        vec_score: None,
        recency_score: None,
        vitality_score: None,
        idf_score: None,
        feedback_adjustment: None,
        rerank_score: None,
    }
}

/// `n` results, ids `m0..mn`, in descending RRF order.
fn ranked(n: usize) -> Vec<MemorySearchResult> {
    (0..n)
        .map(|i| result(&format!("m{i}"), &format!("content {i}"), (n - i) as f64))
        .collect()
}

fn ids(results: &[MemorySearchResult]) -> Vec<String> {
    results.iter().map(|r| r.memory.id.clone()).collect()
}

#[test]
fn availability_matches_the_compiled_feature() {
    assert_eq!(reranker::available(), cfg!(feature = "rerank"));
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn reranking_is_on_by_default() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();

    // Matching the reference, whose REMIND_ME_RERANK defaults to "onnx".
    // "On" here still cannot download anything — see the model tests below.
    assert!(reranker::enabled());
}

#[test]
fn an_empty_backend_turns_it_off() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(reranker::BACKEND_ENV, "");

    assert!(!reranker::enabled());
    assert_eq!(reranker::status(), Status::Disabled);

    clear_env();
}

#[test]
fn top_k_defaults_and_survives_a_bad_value() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    assert_eq!(reranker::top_k_from_env(), reranker::TOP_K_DEFAULT);

    // A typo in a tuning knob must not silently disable the feature — that is
    // REMIND_ME_RERANK's job, and only its job.
    for bad in ["nonsense", "0", "-4", ""] {
        crate::test_env::set_var(reranker::TOP_K_ENV, bad);
        assert_eq!(
            reranker::top_k_from_env(),
            reranker::TOP_K_DEFAULT,
            "{bad:?} should fall back to the default"
        );
    }

    crate::test_env::set_var(reranker::TOP_K_ENV, "5");
    assert_eq!(reranker::top_k_from_env(), 5);

    clear_env();
}

#[test]
fn the_rerank_pool_is_wider_than_the_response_limit() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();

    // Truncating to `limit` first would leave the cross-encoder able only to
    // reorder results that were already going to be returned, discarding the
    // promotion-from-past-the-cutoff that is most of its value.
    assert_eq!(reranker::pool_size(5), reranker::TOP_K_DEFAULT);
    // ...and never *narrower* than the limit either.
    assert_eq!(reranker::pool_size(100), 100);

    clear_env();
}

#[test]
fn an_unconfigured_model_names_both_variables_and_refuses_to_download() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();

    let err = reranker::model_paths().unwrap_err();

    assert!(err.contains(reranker::MODEL_ENV), "{err}");
    assert!(err.contains(reranker::TOKENIZER_ENV), "{err}");
    assert!(err.contains("explicit step"), "{err}");
    // And it says what happens meanwhile, so "nothing appears to have changed"
    // is an expected outcome rather than a mystery.
    assert!(err.contains("RRF order"), "{err}");
}

#[test]
fn a_model_path_that_does_not_exist_names_the_variable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(reranker::MODEL_ENV, "/nonexistent/reranker.rten");
    crate::test_env::set_var(reranker::TOKENIZER_ENV, "/nonexistent/tokenizer.json");

    let err = reranker::model_paths().unwrap_err();
    assert!(err.contains(reranker::MODEL_ENV), "{err}");
    assert!(err.contains("/nonexistent/reranker.rten"), "{err}");

    clear_env();
}

#[test]
fn status_distinguishes_off_from_unconfigured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();

    // The distinction that matters operationally: "you turned it off" and
    // "you left it on but it is doing nothing" look identical in the results
    // and have completely different fixes.
    let on_but_unconfigured = reranker::status();
    assert_ne!(on_but_unconfigured, Status::Disabled);
    assert_ne!(on_but_unconfigured, Status::Ready);

    crate::test_env::set_var(reranker::BACKEND_ENV, "");
    assert_eq!(reranker::status(), Status::Disabled);

    clear_env();
}

// ---------------------------------------------------------------------------
// The ordering contract — no model, no feature, no runtime required
// ---------------------------------------------------------------------------

#[test]
fn the_head_is_reordered_and_the_tail_is_left_alone() {
    let results = ranked(6);

    // Reverses the head of 3; the scorer never sees the tail.
    let out = reranker::rerank_with("q", results, 3, |_, texts| {
        assert_eq!(texts.len(), 3, "the tail must not be scored");
        Ok(vec![1.0, 2.0, 3.0])
    });

    assert_eq!(ids(&out), ["m2", "m1", "m0", "m3", "m4", "m5"]);
}

#[test]
fn ties_keep_their_rrf_order() {
    let results = ranked(4);

    // A cross-encoder that cannot tell two candidates apart must not silently
    // overturn the judgement of the tier that could.
    let out = reranker::rerank_with("q", results, 4, |_, _| Ok(vec![1.0, 1.0, 1.0, 1.0]));

    assert_eq!(ids(&out), ["m0", "m1", "m2", "m3"]);
}

#[test]
fn reranking_never_drops_or_duplicates_a_candidate() {
    let results = ranked(30);
    let before = ids(&results);

    let out = reranker::rerank_with("q", results, 20, |_, texts| {
        Ok((0..texts.len()).map(|i| i as f32).collect())
    });

    assert_eq!(out.len(), before.len());
    let mut sorted_before = before.clone();
    let mut sorted_after = ids(&out);
    sorted_before.sort();
    sorted_after.sort();
    // Reranking permutes a prefix. Anything else — a lost candidate, a
    // repeated one — is a corrupted result page.
    assert_eq!(sorted_before, sorted_after);
    // The tail really is untouched, not merely still present.
    assert_eq!(ids(&out)[20..], before[20..]);
}

#[test]
fn a_top_k_larger_than_the_result_set_is_fine() {
    let results = ranked(3);

    let out = reranker::rerank_with("q", results, 100, |_, texts| {
        assert_eq!(texts.len(), 3);
        Ok(vec![1.0, 3.0, 2.0])
    });

    assert_eq!(ids(&out), ["m1", "m2", "m0"]);
}

#[test]
fn the_score_is_recorded_on_the_head_only() {
    let results = ranked(4);

    let out = reranker::rerank_with("q", results, 2, |_, _| Ok(vec![9.0, 8.0]));

    assert_eq!(out[0].rerank_score, Some(9.0));
    assert_eq!(out[1].rerank_score, Some(8.0));
    // The tail was never scored, so claiming a score for it would be a lie.
    assert_eq!(out[2].rerank_score, None);
    assert_eq!(out[3].rerank_score, None);
}

#[test]
fn the_fused_score_is_not_overwritten() {
    let results = ranked(3);
    let before: Vec<f64> = results.iter().map(|r| r.score).collect();

    let out = reranker::rerank_with("q", results, 3, |_, _| Ok(vec![1.0, 2.0, 3.0]));

    // Reranking permutes; it does not contribute to the fused total. Folding
    // the logit into `score` would double-count the signal and make the
    // diagnostic scores stop adding up.
    let mut after: Vec<f64> = out.iter().map(|r| r.score).collect();
    after.sort_by(|a, b| b.partial_cmp(a).unwrap());
    assert_eq!(after, before);
}

// ---------------------------------------------------------------------------
// Every failure keeps the incoming order
// ---------------------------------------------------------------------------

#[test]
fn a_single_result_is_returned_untouched_without_calling_the_scorer() {
    let out = reranker::rerank_with("q", ranked(1), 20, |_, _| {
        panic!("a one-item list must not cost a model load")
    });
    assert_eq!(ids(&out), ["m0"]);
}

#[test]
fn an_empty_result_set_is_returned_untouched() {
    let out = reranker::rerank_with("q", ranked(0), 20, |_, _| panic!("nothing to score"));
    assert!(out.is_empty());
}

#[test]
fn a_failing_scorer_keeps_the_rrf_order() {
    let out = reranker::rerank_with("q", ranked(5), 5, |_, _| Err("model exploded".into()));

    // Search already worked without a reranker. An enhancement that fails must
    // cost nothing but the enhancement.
    assert_eq!(ids(&out), ["m0", "m1", "m2", "m3", "m4"]);
    assert!(out.iter().all(|r| r.rerank_score.is_none()));
}

#[test]
fn a_scorer_returning_the_wrong_number_of_scores_keeps_the_rrf_order() {
    let out = reranker::rerank_with("q", ranked(5), 5, |_, _| Ok(vec![1.0, 2.0]));

    // Two scores cannot be aligned to five candidates, and guessing an
    // alignment would reorder results by scores belonging to other rows —
    // silently wrong in a way nobody would catch.
    assert_eq!(ids(&out), ["m0", "m1", "m2", "m3", "m4"]);
    assert!(out.iter().all(|r| r.rerank_score.is_none()));
}

// ---------------------------------------------------------------------------
// The integration point
// ---------------------------------------------------------------------------

#[test]
fn maybe_rerank_is_a_no_op_when_turned_off() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(reranker::BACKEND_ENV, "");

    let out = reranker::maybe_rerank("q", ranked(5));
    assert_eq!(ids(&out), ["m0", "m1", "m2", "m3", "m4"]);

    clear_env();
}

#[test]
fn maybe_rerank_is_a_no_op_when_left_on_but_unconfigured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();

    // The default configuration of every build: the setting is on, no model is
    // configured, and search is exactly as it was. This is the path that
    // actually runs for almost everyone, so it is the one that must be a
    // guaranteed no-op rather than a best-effort one.
    let out = reranker::maybe_rerank("q", ranked(5));
    assert_eq!(ids(&out), ["m0", "m1", "m2", "m3", "m4"]);
    assert!(out.iter().all(|r| r.rerank_score.is_none()));
}

// ---------------------------------------------------------------------------
// Feature off — the configuration most builds ship
// ---------------------------------------------------------------------------

#[cfg(not(feature = "rerank"))]
mod without_the_feature {
    use super::*;

    #[test]
    fn status_reports_the_feature_is_missing_and_names_the_flag() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();

        match reranker::status() {
            Status::Unavailable { reason } => {
                assert!(reason.contains("--features rerank"), "{reason}")
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn search_ordering_is_completely_unaffected() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();

        // The acceptance criterion in its plainest form: with the reranker
        // absent, RRF ordering is exactly what it was before this landed.
        let out = reranker::maybe_rerank("q", ranked(40));
        assert_eq!(ids(&out), ids(&ranked(40)));
    }
}

// ---------------------------------------------------------------------------
// Feature on — everything checkable without a downloaded model
// ---------------------------------------------------------------------------

#[cfg(feature = "rerank")]
mod with_the_feature {
    use super::*;

    #[test]
    fn an_unconfigured_search_reranks_nothing_rather_than_downloading_a_model() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();

        // The most important feature-on assertion CI can make: compiling the
        // reranker in must not turn a search into a model download.
        match reranker::status() {
            Status::NotConfigured { reason } => {
                assert!(reason.contains("explicit step"), "{reason}")
            }
            other => panic!("expected NotConfigured, got {other:?}"),
        }

        let out = reranker::maybe_rerank("q", ranked(5));
        assert_eq!(ids(&out), ["m0", "m1", "m2", "m3", "m4"]);
    }

    #[test]
    fn a_model_file_that_is_not_a_model_keeps_the_rrf_order() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();

        let dir = std::env::temp_dir().join("remind_me_rerank_bad_model_test");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("reranker.rten");
        let tokenizer = dir.join("tokenizer.json");
        std::fs::write(&model, b"not a model").unwrap();
        std::fs::write(&tokenizer, b"not a tokenizer").unwrap();
        crate::test_env::set_var(reranker::MODEL_ENV, &model);
        crate::test_env::set_var(reranker::TOKENIZER_ENV, &tokenizer);

        // Configuration looks complete, so this gets all the way to a real
        // load and fails there. Search still has to survive it.
        assert_eq!(reranker::status(), Status::Ready);
        let out = reranker::maybe_rerank("q", ranked(5));
        assert_eq!(ids(&out), ["m0", "m1", "m2", "m3", "m4"]);

        clear_env();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
