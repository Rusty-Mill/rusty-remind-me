//! Optional cross-encoder reranking of the head of the RRF-ranked list.
//!
//! # Why a second scoring stage exists at all
//!
//! RRF fuses *independent* rank lists, so it never reads the query and a
//! candidate together — it only knows that each tier placed a memory at some
//! rank. A cross-encoder does read them together, scoring the pair jointly,
//! which is far more precise at ordering the handful of candidates that
//! actually matter. It is expensive for exactly that reason, which is why only
//! the head is ever rescored.
//!
//! # Reranking may never break search
//!
//! This is the governing rule, and every design choice below follows from it.
//! Search already works without a reranker. So a missing feature, an
//! unconfigured model, an unreadable one, a tokenizer mismatch, an inference
//! failure — every one of them returns the incoming order untouched. None of
//! them is an error, because a search that fails because an *enhancement* was
//! unavailable is worse than a search that is merely ordered less well.
//!
//! # The head is reordered; the tail is preserved
//!
//! [`rerank_with`] rescores the first `top_k` and leaves everything after it
//! exactly where it was, then concatenates. Reranking therefore never *drops*
//! a candidate and never changes how many results come back — it only permutes
//! a prefix. Ties keep their RRF order, because a cross-encoder that cannot
//! distinguish two candidates should not be silently reversing the judgement
//! of the tier that could.
//!
//! # A pool larger than the response limit, rescored before truncation
//!
//! The caller reranks `max(limit, top_k)` candidates and truncates *after*.
//! Truncating first would mean the cross-encoder could only reorder results
//! that were already going to be returned, which throws away most of its
//! value: promoting a candidate from just past the cutoff is the single
//! most useful thing it does.
//!
//! # Enabled by default, but that never means "downloads a model"
//!
//! `REMIND_ME_RERANK` defaults to on, matching the reference. What differs is
//! what "on" costs: the reference fetches a cross-encoder from HuggingFace on
//! first use, whereas here [`MODEL_ENV`] and [`TOKENIZER_ENV`] must point at
//! files that already exist. Unconfigured, reranking is simply a no-op.
//!
//! That combination is deliberate. Defaulting the *setting* on keeps parity
//! for anyone who has a model; requiring an explicit path keeps a search from
//! ever becoming a several-hundred-megabyte download. The one thing it must
//! not do is silently look enabled while doing nothing surprising to the
//! user — hence [`status`], which says which of the two it is.

use crate::models::MemorySearchResult;

/// Backend selector. Empty disables reranking; anything else enables it.
pub const BACKEND_ENV: &str = "REMIND_ME_RERANK";
/// How many head candidates get rescored.
pub const TOP_K_ENV: &str = "REMIND_ME_RERANK_TOP_K";
/// Path to the cross-encoder, in `rten` format.
pub const MODEL_ENV: &str = "REMIND_ME_RERANK_MODEL_PATH";
/// Path to the matching `tokenizer.json`.
pub const TOKENIZER_ENV: &str = "REMIND_ME_RERANK_TOKENIZER_PATH";

/// Matches the reference's `RERANK_TOP_K` default.
pub const TOP_K_DEFAULT: usize = 20;

/// Pairs per forward pass, bounding peak memory on a long result page.
pub const BATCH: usize = 16;

/// Told to the caller when the feature is compiled out.
pub const RERANK_UNAVAILABLE: &str =
    "Cross-encoder reranking is not available in this build: rebuild with the \
     `rerank` feature enabled (cargo build --features rerank).";

/// Told to the caller when the feature is on but no model is configured.
pub const RERANK_NO_MODEL: &str =
    "Cross-encoder reranking needs a model, and it is not downloaded \
     automatically. Set REMIND_ME_RERANK_MODEL_PATH to a cross-encoder in \
     .rten format and REMIND_ME_RERANK_TOKENIZER_PATH to its tokenizer.json. \
     Downloading a model is an explicit step, never something a search does \
     on your behalf. Until then, results keep their RRF order.";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

/// Whether the *setting* asks for reranking.
///
/// Defaults on, matching the reference. Says nothing about whether a model is
/// actually available — see [`status`].
pub fn enabled() -> bool {
    match std::env::var(BACKEND_ENV) {
        Ok(value) => !value.trim().is_empty(),
        Err(_) => true,
    }
}

/// Whether this build can rerank at all.
pub fn available() -> bool {
    cfg!(feature = "rerank")
}

/// How many head candidates to rescore.
///
/// A malformed or zero value falls back to the default rather than disabling
/// reranking: `REMIND_ME_RERANK` is the switch, and a typo in an unrelated
/// tuning knob should not silently turn a feature off.
pub fn top_k_from_env() -> usize {
    match env(TOP_K_ENV).parse::<usize>() {
        Ok(k) if k > 0 => k,
        _ => TOP_K_DEFAULT,
    }
}

/// How many candidates a caller should rerank before truncating to `limit`.
///
/// Larger than the response limit on purpose: rescoring only what was already
/// going to be returned would discard the most useful thing a cross-encoder
/// does, which is promote a candidate from just past the cutoff.
pub fn pool_size(limit: usize) -> usize {
    limit.max(top_k_from_env())
}

/// The configured model and tokenizer paths, or why they are unusable.
///
/// A pure function of the environment, so the configuration decision is
/// testable without the feature compiled in and without a model on disk.
pub fn model_paths() -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let model = env(MODEL_ENV);
    let tokenizer = env(TOKENIZER_ENV);
    if model.is_empty() || tokenizer.is_empty() {
        return Err(RERANK_NO_MODEL.to_string());
    }
    let (model, tokenizer) = (
        std::path::PathBuf::from(model),
        std::path::PathBuf::from(tokenizer),
    );
    for (var, path) in [(MODEL_ENV, &model), (TOKENIZER_ENV, &tokenizer)] {
        if !path.exists() {
            return Err(format!(
                "{} points at {}, which does not exist.",
                var,
                path.display()
            ));
        }
    }
    Ok((model, tokenizer))
}

/// Why reranking is or is not going to happen.
///
/// Exists because "enabled but silently doing nothing" is the one outcome that
/// would be genuinely confusing: search would look configured and behave
/// exactly as if it were not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Turned off by `REMIND_ME_RERANK`.
    Disabled,
    /// Wanted, but this build cannot.
    Unavailable {
        reason: String,
    },
    /// Wanted and possible, but no usable model is configured.
    NotConfigured {
        reason: String,
    },
    Ready,
}

/// Report whether reranking will run, and why not when it will not.
pub fn status() -> Status {
    if !enabled() {
        return Status::Disabled;
    }
    if !available() {
        return Status::Unavailable {
            reason: RERANK_UNAVAILABLE.to_string(),
        };
    }
    match model_paths() {
        Ok(_) => Status::Ready,
        Err(reason) => Status::NotConfigured { reason },
    }
}

/// Reorder the first `top_k` results by `scorer`, keeping the tail as-is.
///
/// The scorer is injected rather than reached for, which is what makes the
/// whole ordering contract — the head/tail split, tie stability, the recorded
/// score, the degenerate cases — testable with no model, no feature and no
/// runtime. That is the part most likely to be quietly wrong, so it is the
/// part held to account unconditionally.
///
/// A scorer that fails, or returns the wrong number of scores, leaves the
/// input order untouched.
pub fn rerank_with<F>(
    query: &str,
    mut results: Vec<MemorySearchResult>,
    top_k: usize,
    scorer: F,
) -> Vec<MemorySearchResult>
where
    F: FnOnce(&str, &[String]) -> Result<Vec<f32>, String>,
{
    let head_len = top_k.min(results.len());
    // Nothing to reorder. Matching the reference, which returns early rather
    // than paying for a model load to sort one item.
    if head_len < 2 {
        return results;
    }

    let texts: Vec<String> = results[..head_len]
        .iter()
        .map(|r| r.memory.content.clone())
        .collect();

    let Ok(scores) = scorer(query, &texts) else {
        return results;
    };
    // A scorer that returned a different number of scores than it was given
    // texts cannot be aligned to candidates, and guessing an alignment would
    // silently reorder results by the wrong scores.
    if scores.len() != head_len {
        return results;
    }

    for (result, score) in results[..head_len].iter_mut().zip(scores.iter()) {
        result.rerank_score = Some(*score as f64);
    }
    // `sort_by` is stable, so equal scores keep their RRF order rather than
    // being permuted arbitrarily.
    results[..head_len].sort_by(|a, b| {
        b.rerank_score
            .unwrap_or(f64::MIN)
            .partial_cmp(&a.rerank_score.unwrap_or(f64::MIN))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// The single integration point for search.
///
/// Returns the input untouched whenever reranking is off, unavailable,
/// unconfigured or fails. Never errors.
pub fn maybe_rerank(query: &str, results: Vec<MemorySearchResult>) -> Vec<MemorySearchResult> {
    if results.len() < 2 || !matches!(status(), Status::Ready) {
        return results;
    }
    let top_k = top_k_from_env();
    rerank_with(query, results, top_k, |query, texts| {
        backend::score(query, texts)
    })
}

#[cfg(feature = "rerank")]
mod backend {
    use super::*;
    use rten::{Model, NodeId, ValueOrView};
    use rten_tensor::prelude::*;
    use std::sync::{Arc, Mutex, OnceLock};
    use tokenizers::Tokenizer;

    struct Engine {
        model: Model,
        tokenizer: Tokenizer,
    }

    /// The loaded model, remembered along with the paths it came from.
    ///
    /// A cross-encoder is hundreds of megabytes; reloading it per search would
    /// cost far more than the reranking saves. Keyed on the paths rather than
    /// loaded once forever, because they are configuration and can change.
    type Cached = (std::path::PathBuf, std::path::PathBuf, Arc<Engine>);
    fn cache() -> &'static Mutex<Option<Cached>> {
        static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(None))
    }

    fn engine() -> Result<Arc<Engine>, String> {
        let (model_path, tokenizer_path) = model_paths()?;

        let mut cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((model, tokenizer, engine)) = cached.as_ref() {
            if model == &model_path && tokenizer == &tokenizer_path {
                return Ok(Arc::clone(engine));
            }
        }

        let model = Model::load_file(&model_path)
            .map_err(|e| format!("could not load {}: {}", model_path.display(), e))?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| format!("could not load {}: {}", tokenizer_path.display(), e))?;

        let engine = Arc::new(Engine { model, tokenizer });
        *cached = Some((model_path, tokenizer_path, Arc::clone(&engine)));
        Ok(engine)
    }

    /// Score `(query, text)` pairs jointly; higher means more relevant.
    ///
    /// Logits are monotonic in relevance, so they sort directly and are not
    /// normalised — a softmax here would cost time and change nothing about
    /// the ordering.
    pub fn score(query: &str, texts: &[String]) -> Result<Vec<f32>, String> {
        let engine = engine()?;
        let mut all = Vec::with_capacity(texts.len());

        for batch in texts.chunks(BATCH) {
            let pairs: Vec<(String, String)> = batch
                .iter()
                .map(|text| (query.to_string(), text.clone()))
                .collect();
            let encoded = engine
                .tokenizer
                .encode_batch(pairs, true)
                .map_err(|e| format!("could not tokenize: {}", e))?;

            let rows = encoded.len();
            // Padded to the longest row in the batch: a rectangular tensor is
            // required, and the attention mask is what tells the model which
            // of those positions are padding.
            let width = encoded
                .iter()
                .map(|e| e.get_ids().len())
                .max()
                .unwrap_or_default();
            if rows == 0 || width == 0 {
                continue;
            }

            let mut ids = Vec::with_capacity(rows * width);
            let mut mask = Vec::with_capacity(rows * width);
            let mut types = Vec::with_capacity(rows * width);
            for item in &encoded {
                for column in 0..width {
                    ids.push(*item.get_ids().get(column).unwrap_or(&0) as i32);
                    mask.push(*item.get_attention_mask().get(column).unwrap_or(&0) as i32);
                    types.push(*item.get_type_ids().get(column).unwrap_or(&0) as i32);
                }
            }
            let ids = rten_tensor::NdTensor::from_data([rows, width], ids);
            let mask = rten_tensor::NdTensor::from_data([rows, width], mask);
            let types = rten_tensor::NdTensor::from_data([rows, width], types);

            let mut inputs: Vec<(NodeId, ValueOrView)> = Vec::new();
            let node = |name: &str| {
                engine
                    .model
                    .find_node(name)
                    .ok_or_else(|| format!("the model has no `{}` input", name))
            };
            inputs.push((node("input_ids")?, ids.view().into()));
            inputs.push((node("attention_mask")?, mask.view().into()));
            // Some exports omit segment ids; feeding an input the graph does
            // not declare is an error, so this is fed only when declared.
            if let Some(node) = engine.model.find_node("token_type_ids") {
                inputs.push((node, types.view().into()));
            }

            let output_id = *engine
                .model
                .output_ids()
                .first()
                .ok_or("the model declares no outputs")?;
            let outputs = engine
                .model
                .run(inputs, &[output_id], None)
                .map_err(|e| format!("reranker inference failed: {}", e))?;
            let logits: rten_tensor::Tensor<f32> = outputs
                .into_iter()
                .next()
                .ok_or("the model returned no output")?
                .try_into()
                .map_err(|e| format!("unexpected reranker output: {}", e))?;

            // A single-logit relevance head emits one score per row; a
            // two-logit head emits (irrelevant, relevant). Taking the last
            // value of each row reads both correctly.
            let flat: Vec<f32> = logits.iter().copied().collect();
            if !flat.len().is_multiple_of(rows) {
                return Err(format!(
                    "reranker returned {} values for {} candidates",
                    flat.len(),
                    rows
                ));
            }
            let stride = flat.len() / rows;
            for row in 0..rows {
                all.push(flat[row * stride + stride - 1]);
            }
        }
        Ok(all)
    }
}

#[cfg(not(feature = "rerank"))]
mod backend {
    pub fn score(_query: &str, _texts: &[String]) -> Result<Vec<f32>, String> {
        Err(super::RERANK_UNAVAILABLE.to_string())
    }
}
