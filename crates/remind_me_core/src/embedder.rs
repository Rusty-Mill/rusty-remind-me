//! Turning text into vectors: chunking, the embedding backend(s), and
//! graceful degradation when none is configured.
//!
//! Two backends, both matching the reference's own two supported backends
//! (`embeddings.py`'s `OllamaEmbedder` and `_Embedder`):
//!
//! - `ollama` ([`OllamaEmbedder`]): Ollama's HTTP `/api/embed`. No compile-time
//!   ML dependency, works with whatever retriever the operator already
//!   pulled into Ollama.
//! - `onnx` ([`OnnxEmbedder`], behind the `local-embed` feature): an
//!   in-process ONNX bi-encoder via `rten` (the same pure-Rust runtime
//!   `reranker`/`ocr` use), mean-pooled and L2-normalised the same way the
//!   reference's ONNX Runtime path is. Like `reranker`, it takes explicit
//!   `.rten` model/tokenizer paths — enabling it can never turn a search
//!   into an implicit HuggingFace download. See
//!   `docs/adr/0002-embeddings-ollama-and-brute-force-vectors.md` for why
//!   Ollama was the *first* backend built (small, auditable, zero new
//!   compile-time dependencies); this module's own `Embedder` trait is what
//!   made adding the second one additive rather than a rewrite.
//!
//! # Off unless configured
//!
//! [`resolve_embedder`] returns `None` unless `REMIND_ME_EMBEDDING_BACKEND`
//! is `ollama` or `onnx` — matching the folder watcher (#55) and webhook
//! (#56) convention of "the risky/heavy thing stays off until asked for." A
//! caller that gets `None` degrades to keyword-only search; nothing here
//! ever makes that an error.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// `REMIND_ME_EMBEDDING_BACKEND` must be `ollama` or `onnx` to enable
/// semantic search.
pub const EMBEDDING_BACKEND_ENV: &str = "REMIND_ME_EMBEDDING_BACKEND";
pub const OLLAMA_URL_ENV: &str = "REMIND_ME_OLLAMA_URL";
pub const OLLAMA_MODEL_ENV: &str = "REMIND_ME_OLLAMA_EMBED_MODEL";
pub const EMBEDDING_DIM_ENV: &str = "REMIND_ME_EMBEDDING_DIM";
/// Path to an ONNX bi-encoder, in `rten` format — see `reranker`'s
/// `REMIND_ME_RERANK_MODEL_PATH` for the same convention and the same
/// reason (no implicit download).
pub const ONNX_MODEL_PATH_ENV: &str = "REMIND_ME_ONNX_MODEL_PATH";
/// Path to the matching `tokenizer.json`.
pub const ONNX_TOKENIZER_PATH_ENV: &str = "REMIND_ME_ONNX_TOKENIZER_PATH";

pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
pub const DEFAULT_OLLAMA_MODEL: &str = "nomic-embed-text";
/// Matches the reference's own default, sized for
/// `sentence-transformers/all-MiniLM-L6-v2` — the reference's default ONNX
/// model and this crate's own `onnx` backend's expected dimension when
/// pointed at that same model. Anyone using a different-dimensioned model
/// (Ollama or ONNX) sets this to match, exactly as the reference requires.
pub const DEFAULT_EMBEDDING_DIM: usize = 384;

/// Told to the caller when `onnx` is requested but the `local-embed`
/// feature was not compiled in.
pub const ONNX_UNAVAILABLE: &str =
    "Local ONNX embedding is not available in this build: rebuild with the \
     `local-embed` feature enabled (cargo build --features remind_me_core/local-embed).";
/// Told to the caller when the feature is on but no model is configured.
pub const ONNX_NO_MODEL: &str = "Local ONNX embedding needs a model, and it is not downloaded \
     automatically. Set REMIND_ME_ONNX_MODEL_PATH to a bi-encoder in .rten \
     format and REMIND_ME_ONNX_TOKENIZER_PATH to its tokenizer.json.";

/// Chunking defaults, matching `embeddings.py`'s `EMBED_CHUNK_*` exactly.
pub const EMBED_CHUNK_CHARS: usize = 1600;
pub const EMBED_CHUNK_OVERLAP: usize = 200;
pub const EMBED_MAX_CHUNKS: usize = 16;
/// Texts per HTTP call, bounding request size the same way the reference
/// bounds its forward-pass batch size.
pub const EMBED_FORWARD_BATCH: usize = 32;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether an embedding is for a search query or an indexed passage.
///
/// Some model families were trained with an asymmetric instruction prefix
/// (`nomic-embed-text`'s `search_query:`/`search_document:`, `e5-*`'s
/// `query:`/`passage:`, BGE's query-only prefix) — searching unprefixed
/// still works, but noticeably worse than the model's own convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedRole {
    Query,
    Passage,
}

/// Model-family instruction prefixes, matched by substring against the
/// lowercased model name — so both a full HuggingFace/Ollama repo path and a
/// short tag resolve to the same entry. A model absent from this table gets
/// no prefix.
const ROLE_PREFIXES: &[(&str, &str, &str)] = &[
    ("nomic-embed-text", "search_query: ", "search_document: "),
    ("e5-", "query: ", "passage: "),
    (
        "bge-",
        "Represent this sentence for searching relevant passages: ",
        "",
    ),
];

fn prefix_for(model_name: &str, role: EmbedRole) -> &'static str {
    let name = model_name.to_lowercase();
    for (key, query_prefix, passage_prefix) in ROLE_PREFIXES {
        if name.contains(key) {
            return match role {
                EmbedRole::Query => query_prefix,
                EmbedRole::Passage => passage_prefix,
            };
        }
    }
    ""
}

/// Why embedding failed. Every variant is something a caller degrades on —
/// there is no case here that should ever become a panic.
#[derive(Debug)]
pub struct EmbedError(pub String);

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EmbedError {}

/// Something that turns text into vectors.
///
/// A trait, not a single concrete type, so a future backend (ONNX-in-process,
/// say) can be added without touching anything that already depends on this.
pub trait Embedder {
    /// Embed a batch of texts, returning one L2-normalised `f32` vector per
    /// text, in the same order. Empty `texts` returns an empty result rather
    /// than an error.
    fn embed(&self, texts: &[String], role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// The dimension this embedder's vectors are expected to have.
    fn dim(&self) -> usize;

    /// The backend/model/dimension triple that identifies which model this
    /// embedder is, for embedding-model versioning (#96) — recorded in
    /// `embedding_meta` once vectors are actually written, so a later
    /// mismatch against whatever is currently configured can be detected.
    /// Matches how the reference identifies "the model" for the same
    /// purpose: its `EMBEDDING_BACKEND`/`EMBEDDING_MODEL`/`EMBEDDING_DIM`
    /// triple.
    fn identity(&self) -> EmbeddingIdentity;
}

/// Which backend, model, and dimension produced (or would produce) a given
/// batch of vectors — see [`Embedder::identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingIdentity {
    pub backend: String,
    pub model: String,
    pub dim: usize,
}

// ---------------------------------------------------------------------------
// Sliding-window chunking
// ---------------------------------------------------------------------------

/// Split text into overlapping character windows for multi-vector embedding.
///
/// Content at or under `max_chars` returns a single chunk `[text]`, so short
/// memories embed exactly as they would unchunked. Longer content is cut
/// into windows of up to `max_chars` characters overlapping by `overlap`, so
/// evidence straddling a boundary still lands whole in at least one window.
/// Each cut prefers the nearest space before the limit, so a window does not
/// split mid-word. At most `max_chunks` windows are produced; any remaining
/// tail is dropped.
///
/// Operates on `char`s, not bytes — matching the reference's Python (whose
/// `str` indexing is by code point), which matters the moment any content
/// has a multi-byte character near a cut point.
pub fn chunk_text(text: &str, max_chars: usize, overlap: usize, max_chunks: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let n = chars.len();
    if n <= max_chars {
        return vec![trimmed.to_string()];
    }

    // Clamped so the window always advances.
    let step = max_chars
        .saturating_sub(overlap.min(max_chars.saturating_sub(1)))
        .max(1);

    let rfind_space = |from: usize, to: usize| -> Option<usize> {
        if from >= to {
            return None;
        }
        (from..to).rev().find(|&i| chars[i] == ' ')
    };

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < n && chunks.len() < max_chunks {
        let mut end = (start + max_chars).min(n);
        if end < n {
            if let Some(ws) = rfind_space(start + step, end) {
                end = ws;
            }
        }
        let chunk: String = chars[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        if end >= n {
            break;
        }
        let next_start = if end.saturating_sub(overlap) > start {
            end - overlap
        } else {
            end
        };
        start = match rfind_space(start, next_start) {
            Some(sp) if sp > start => sp + 1,
            _ => next_start,
        };
    }
    chunks
}

// ---------------------------------------------------------------------------
// Ollama backend
// ---------------------------------------------------------------------------

/// Embeds via a local Ollama daemon's `POST /api/embed`.
///
/// Calls Ollama's batch embedding endpoint instead of running a model
/// in-process — no HuggingFace download, no compile-time ML dependency, and
/// it unlocks whichever retriever the operator has already pulled into
/// Ollama (`nomic-embed-text`, `bge-m3`, ...).
///
/// The returned vector length **must** equal `dim` (baked into every stored
/// vector's meaning): a mismatch is a configuration error, not a silent
/// truncation or pad, so it is surfaced clearly rather than corrupting the
/// index.
pub struct OllamaEmbedder {
    pub model: String,
    /// e.g. `http://localhost:11434`, no trailing slash.
    pub url: String,
    pub dim: usize,
}

impl OllamaEmbedder {
    pub fn new(model: impl Into<String>, url: impl Into<String>, dim: usize) -> Self {
        Self {
            model: model.into(),
            url: url.into().trim_end_matches('/').to_string(),
            dim,
        }
    }

    fn host_port(&self) -> Result<(String, u16), EmbedError> {
        let rest = self.url.strip_prefix("http://").ok_or_else(|| {
            EmbedError(format!(
                "only http:// Ollama URLs are supported, got {:?}",
                self.url
            ))
        })?;
        let (host, port) = rest
            .split_once(':')
            .ok_or_else(|| EmbedError(format!("Ollama URL has no port: {:?}", self.url)))?;
        let port: u16 = port
            .parse()
            .map_err(|_| EmbedError(format!("invalid port in Ollama URL: {:?}", self.url)))?;
        Ok((host.to_string(), port))
    }

    fn post_json(&self, path: &str, body: &str) -> Result<String, EmbedError> {
        let (host, port) = self.host_port()?;
        // `connect_timeout` needs a concrete `SocketAddr`, so a hostname
        // (e.g. "localhost") is resolved first — this also transparently
        // covers an IP literal, which resolves to itself.
        use std::net::ToSocketAddrs;
        let addr = (host.as_str(), port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .ok_or_else(|| EmbedError(format!("cannot resolve Ollama host {:?}", host)))?;
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| EmbedError(format!("cannot reach Ollama at {}: {}", self.url, e)))?;
        stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
        stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

        let request = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            path,
            host,
            body.len(),
            body
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| EmbedError(format!("writing to Ollama failed: {}", e)))?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| EmbedError(format!("reading from Ollama failed: {}", e)))?;
        let text = String::from_utf8_lossy(&raw);
        let (head, response_body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| EmbedError("malformed HTTP response from Ollama".to_string()))?;
        let status: u16 = head
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| EmbedError("malformed HTTP status line from Ollama".to_string()))?;
        if !(200..300).contains(&status) {
            return Err(EmbedError(format!(
                "Ollama returned {}: {}",
                status,
                response_body.trim()
            )));
        }
        Ok(response_body.to_string())
    }
}

impl Embedder for OllamaEmbedder {
    fn embed(&self, texts: &[String], role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prefix = prefix_for(&self.model, role);
        let prefixed: Vec<String> = if prefix.is_empty() {
            texts.to_vec()
        } else {
            texts.iter().map(|t| format!("{}{}", prefix, t)).collect()
        };

        let mut all = Vec::with_capacity(prefixed.len());
        for batch in prefixed.chunks(EMBED_FORWARD_BATCH) {
            let body = serde_json::json!({ "model": self.model, "input": batch }).to_string();
            let response = self.post_json("/api/embed", &body)?;
            let parsed: serde_json::Value = serde_json::from_str(&response)
                .map_err(|e| EmbedError(format!("Ollama response was not JSON: {}", e)))?;
            let vectors = parsed
                .get("embeddings")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    EmbedError(format!(
                        "Ollama returned no embeddings for model {:?}",
                        self.model
                    ))
                })?;
            if vectors.is_empty() {
                return Err(EmbedError(format!(
                    "Ollama returned no embeddings for model {:?}",
                    self.model
                )));
            }
            for vector in vectors {
                let raw: Vec<f32> = vector
                    .as_array()
                    .ok_or_else(|| {
                        EmbedError("Ollama embedding entry was not an array".to_string())
                    })?
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect();
                if raw.len() != self.dim {
                    return Err(EmbedError(format!(
                        "Ollama model {:?} returned dim {}, but {} is {}. Set the dimension to \
                         match the model and run remind_me_reindex on a fresh vector table.",
                        self.model,
                        raw.len(),
                        EMBEDDING_DIM_ENV,
                        self.dim
                    )));
                }
                all.push(l2_normalize(raw));
            }
        }
        Ok(all)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "ollama".to_string(),
            model: self.model.clone(),
            dim: self.dim,
        }
    }
}

// ---------------------------------------------------------------------------
// ONNX backend (in-process, behind `local-embed`)
// ---------------------------------------------------------------------------

/// Embeds via an in-process ONNX bi-encoder (`rten`), no daemon required.
///
/// Mean-pools the last hidden state over real (non-padding) tokens and
/// L2-normalises — the same pipeline the reference's `_Embedder._embed_forward`
/// runs through ONNX Runtime. Model/tokenizer are loaded lazily on first use
/// and cached process-wide, the same pattern [`crate::reranker`]'s backend
/// uses for the same reason: reloading a model per search would cost far
/// more than embedding saves.
pub struct OnnxEmbedder {
    pub model_path: std::path::PathBuf,
    pub tokenizer_path: std::path::PathBuf,
    pub dim: usize,
}

impl OnnxEmbedder {
    pub fn new(
        model_path: impl Into<std::path::PathBuf>,
        tokenizer_path: impl Into<std::path::PathBuf>,
        dim: usize,
    ) -> Self {
        Self {
            model_path: model_path.into(),
            tokenizer_path: tokenizer_path.into(),
            dim,
        }
    }
}

impl Embedder for OnnxEmbedder {
    fn embed(&self, texts: &[String], role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // No natural "model name" for a local file path; keyed on the path's
        // text anyway so a future model whose HuggingFace repo name appears
        // in the path (e.g. ".../models--intfloat--e5-base-v2/...") still
        // picks up its convention. The reference's own ONNX default
        // (all-MiniLM-L6-v2) has none either way.
        let model_name = self.model_path.to_string_lossy();
        let prefix = prefix_for(&model_name, role);
        let prefixed: Vec<String> = if prefix.is_empty() {
            texts.to_vec()
        } else {
            texts.iter().map(|t| format!("{prefix}{t}")).collect()
        };

        let mut all = Vec::with_capacity(prefixed.len());
        for batch in prefixed.chunks(EMBED_FORWARD_BATCH) {
            let vectors = onnx_backend::embed(&self.model_path, &self.tokenizer_path, batch)
                .map_err(EmbedError)?;
            for v in vectors {
                if v.len() != self.dim {
                    return Err(EmbedError(format!(
                        "ONNX model at {:?} produced dim {}, but {} is {}. Set the dimension to \
                         match the model and run remind_me_reindex on a fresh vector table.",
                        self.model_path,
                        v.len(),
                        EMBEDDING_DIM_ENV,
                        self.dim
                    )));
                }
                all.push(v);
            }
        }
        Ok(all)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "onnx".to_string(),
            model: self.model_path.display().to_string(),
            dim: self.dim,
        }
    }
}

#[cfg(feature = "local-embed")]
mod onnx_backend {
    use rten::{Model, NodeId, ValueOrView};
    use rten_tensor::prelude::*;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};
    use tokenizers::Tokenizer;

    struct Engine {
        model: Model,
        tokenizer: Tokenizer,
    }

    /// The loaded model, remembered along with the paths it came from — the
    /// same cache shape [`crate::reranker`]'s backend uses.
    type Cached = (PathBuf, PathBuf, Arc<Engine>);
    fn cache() -> &'static Mutex<Option<Cached>> {
        static CACHE: LazyLock<Mutex<Option<Cached>>> = LazyLock::new(|| Mutex::new(None));
        &CACHE
    }

    fn engine(model_path: &Path, tokenizer_path: &Path) -> Result<Arc<Engine>, String> {
        let mut cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((model, tokenizer, engine)) = cached.as_ref() {
            if model == model_path && tokenizer == tokenizer_path {
                return Ok(Arc::clone(engine));
            }
        }
        let model = Model::load_file(model_path)
            .map_err(|e| format!("could not load {}: {}", model_path.display(), e))?;
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| format!("could not load {}: {}", tokenizer_path.display(), e))?;
        let engine = Arc::new(Engine { model, tokenizer });
        *cached = Some((
            model_path.to_path_buf(),
            tokenizer_path.to_path_buf(),
            Arc::clone(&engine),
        ));
        Ok(engine)
    }

    /// Mean-pool the last hidden state over non-padding tokens and
    /// L2-normalise, one vector per input row. Empty `texts` (already
    /// rejected by the caller) or an all-padding batch returns `Ok(vec![])`
    /// rather than dividing by zero.
    pub fn embed(
        model_path: &Path,
        tokenizer_path: &Path,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, String> {
        let engine = engine(model_path, tokenizer_path)?;
        let encoded = engine
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| format!("could not tokenize: {}", e))?;

        let rows = encoded.len();
        let width = encoded
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or_default();
        if rows == 0 || width == 0 {
            return Ok(Vec::new());
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
        let ids_t = rten_tensor::NdTensor::from_data([rows, width], ids);
        let mask_t = rten_tensor::NdTensor::from_data([rows, width], mask.clone());
        let types_t = rten_tensor::NdTensor::from_data([rows, width], types);

        let mut inputs: Vec<(NodeId, ValueOrView)> = Vec::new();
        let node = |name: &str| {
            engine
                .model
                .find_node(name)
                .ok_or_else(|| format!("the model has no `{}` input", name))
        };
        inputs.push((node("input_ids")?, ids_t.view().into()));
        inputs.push((node("attention_mask")?, mask_t.view().into()));
        if let Some(node) = engine.model.find_node("token_type_ids") {
            inputs.push((node, types_t.view().into()));
        }

        let output_id = *engine
            .model
            .output_ids()
            .first()
            .ok_or("the model declares no outputs")?;
        let outputs = engine
            .model
            .run(inputs, &[output_id], None)
            .map_err(|e| format!("embedding inference failed: {}", e))?;
        // Last hidden state, shape (rows, width, dim), row-major. `dim` is
        // inferred from the flat length rather than a `.shape()` call, the
        // same defensive style `reranker`'s backend uses.
        let hidden: rten_tensor::Tensor<f32> = outputs
            .into_iter()
            .next()
            .ok_or("the model returned no output")?
            .try_into()
            .map_err(|e| format!("unexpected embedding output: {}", e))?;
        let flat: Vec<f32> = hidden.iter().copied().collect();
        if rows == 0 || width == 0 || !flat.len().is_multiple_of(rows * width) {
            return Err(format!(
                "unexpected embedding output size {} for {}x{} tokens",
                flat.len(),
                rows,
                width
            ));
        }
        let dim = flat.len() / (rows * width);
        if dim == 0 {
            return Err("embedding model returned zero-dimensional output".to_string());
        }

        let mut result = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut pooled = vec![0.0f32; dim];
            let mut count = 0.0f32;
            for col in 0..width {
                if mask[row * width + col] == 0 {
                    continue;
                }
                count += 1.0;
                let base = (row * width + col) * dim;
                for (d, slot) in pooled.iter_mut().enumerate() {
                    *slot += flat[base + d];
                }
            }
            let denom = count.max(1e-9);
            for v in pooled.iter_mut() {
                *v /= denom;
            }
            result.push(super::l2_normalize(pooled));
        }
        Ok(result)
    }
}

#[cfg(not(feature = "local-embed"))]
mod onnx_backend {
    use std::path::Path;
    pub fn embed(
        _model_path: &Path,
        _tokenizer_path: &Path,
        _texts: &[String],
    ) -> Result<Vec<Vec<f32>>, String> {
        Err(super::ONNX_UNAVAILABLE.to_string())
    }
}

pub(crate) fn l2_normalize(vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    vector.into_iter().map(|x| x / norm).collect()
}

// ---------------------------------------------------------------------------
// Availability cache and resolution
// ---------------------------------------------------------------------------

/// Seconds a successful availability probe stays cached, matching the
/// reference's own `AVAILABILITY_SUCCESS_TTL` — so the hot search path
/// doesn't pay a real ping round-trip on every call.
const AVAILABILITY_SUCCESS_TTL: Duration = Duration::from_secs(60);
/// Matches `AVAILABILITY_FAILURE_TTL` — a failed probe is retried sooner
/// than a successful one is re-checked.
const AVAILABILITY_FAILURE_TTL: Duration = Duration::from_secs(30);

struct AvailabilityCache {
    /// Which configuration this verdict was reached about — see
    /// [`config_fingerprint`]. A cached verdict only applies while the
    /// configuration it was probed under is still the configuration in force.
    fingerprint: String,
    available: bool,
    expires_at: Instant,
}

static AVAILABILITY: OnceLock<Mutex<Option<AvailabilityCache>>> = OnceLock::new();

fn availability_cell() -> &'static Mutex<Option<AvailabilityCache>> {
    AVAILABILITY.get_or_init(|| Mutex::new(None))
}

/// Every environment variable [`resolve_embedder`] reads, joined into one
/// value that changes whenever the resolved embedder would.
///
/// This exists because [`available_embedder`]'s cache is keyed on it, and
/// that is what keeps `resolve_embedder`'s documented promise — "a
/// configuration change takes effect on the next search rather than requiring
/// a restart" — true of the *availability-gated* resolver too. Without it a
/// verdict reached about one backend answered for a different one: point the
/// URL at a daemon that is actually up, and `remind_me_server_status` would
/// keep reporting "configured but not answering" for the rest of
/// [`AVAILABILITY_FAILURE_TTL`], because nothing invalidated the failure the
/// old address earned.
///
/// The list must stay in step with `resolve_embedder`. A variable it reads
/// but this omits is a variable you can change without the cache noticing —
/// which is the bug this function exists to close, reintroduced quietly.
fn config_fingerprint() -> String {
    [
        EMBEDDING_BACKEND_ENV,
        EMBEDDING_DIM_ENV,
        OLLAMA_URL_ENV,
        OLLAMA_MODEL_ENV,
        ONNX_MODEL_PATH_ENV,
        ONNX_TOKENIZER_PATH_ENV,
    ]
    .iter()
    // A unit separator rather than a comma or a space: it cannot occur in a
    // URL or a filesystem path, so no two distinct configurations can join
    // into the same string.
    .map(|key| std::env::var(key).unwrap_or_default())
    .collect::<Vec<_>>()
    .join("\u{1f}")
}

/// Build the configured embedder, or `None` when nothing is configured.
///
/// Reads the environment fresh on every call — cheap, and it means a
/// configuration change takes effect on the next search rather than
/// requiring a restart. Returns `None` when `REMIND_ME_EMBEDDING_BACKEND` is
/// anything other than `ollama`/`onnx`; that is the disabled state, not an
/// error.
pub fn resolve_embedder() -> Option<Box<dyn Embedder>> {
    let backend = std::env::var(EMBEDDING_BACKEND_ENV).unwrap_or_default();
    let dim = std::env::var(EMBEDDING_DIM_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_EMBEDDING_DIM);
    match backend.as_str() {
        "ollama" => {
            let url =
                std::env::var(OLLAMA_URL_ENV).unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());
            let model = std::env::var(OLLAMA_MODEL_ENV)
                .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
            Some(Box::new(OllamaEmbedder::new(model, url, dim)))
        }
        "onnx" => {
            let model_path = std::env::var(ONNX_MODEL_PATH_ENV).unwrap_or_default();
            let tokenizer_path = std::env::var(ONNX_TOKENIZER_PATH_ENV).unwrap_or_default();
            // Unlike Ollama, an empty path is never usable, but resolving is
            // still config-only here: `available_embedder`'s probe (below)
            // is what turns "onnx requested but unconfigured" into the same
            // "unavailable" state an unreachable Ollama daemon gets, via the
            // same mechanism rather than a second special case.
            Some(Box::new(OnnxEmbedder::new(model_path, tokenizer_path, dim)))
        }
        _ => None,
    }
}

/// The configured embedder, but only if it actually answers — the
/// availability-gated resolver every search path should use.
///
/// A failed probe is cached for [`AVAILABILITY_FAILURE_TTL`] and a
/// successful one for [`AVAILABILITY_SUCCESS_TTL`], so neither an
/// unreachable daemon nor a reachable one is re-probed on every search.
///
/// The cache is keyed on [`config_fingerprint`], so a verdict expires either
/// when its TTL runs out **or** when the configuration it was reached about
/// changes — whichever happens first. Time alone was not enough: a verdict
/// about one daemon has nothing to say about a different one, and treating it
/// as though it did made a corrected configuration look broken for up to
/// thirty seconds after it started working.
pub fn available_embedder() -> Option<Box<dyn Embedder>> {
    let embedder = resolve_embedder()?;
    let fingerprint = config_fingerprint();

    let cache = availability_cell();
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.as_ref() {
            if entry.fingerprint == fingerprint && Instant::now() < entry.expires_at {
                return entry.available.then_some(embedder);
            }
        }
    }

    let ok = embedder
        .embed(&["ping".to_string()], EmbedRole::Query)
        .is_ok();
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(AvailabilityCache {
        fingerprint,
        available: ok,
        expires_at: Instant::now()
            + if ok {
                AVAILABILITY_SUCCESS_TTL
            } else {
                AVAILABILITY_FAILURE_TTL
            },
    });
    ok.then_some(embedder)
}

/// What `remind_me_server_status` reports for the embedding backend.
///
/// Unlike [`resolve_embedder`] (config only) this calls [`available_embedder`],
/// which makes a network probe — cached, per `AVAILABILITY_SUCCESS_TTL`/
/// `AVAILABILITY_FAILURE_TTL` above, so this does not cost a real
/// round-trip on every status check. `crate::status::server_status` never
/// calls this itself (its own "no network" contract, see that module's
/// docs); it is for the MCP dispatch layer's live-override of `embeddings`,
/// the same way `sync`/`webhook`/`remote` are overridden with process-local
/// state `server_status` cannot see.
pub fn embedding_status() -> crate::status::SubsystemStatus {
    use crate::status::SubsystemStatus;
    if resolve_embedder().is_none() {
        return SubsystemStatus::NotImplemented {
            reason: "no embedding backend configured; set REMIND_ME_EMBEDDING_BACKEND=ollama \
                     or REMIND_ME_EMBEDDING_BACKEND=onnx to enable semantic search"
                .to_string(),
        };
    }
    if available_embedder().is_some() {
        SubsystemStatus::Active
    } else {
        SubsystemStatus::NotImplemented {
            reason: format!(
                "REMIND_ME_EMBEDDING_BACKEND={} is configured but not answering (unreachable \
                 daemon, or an unconfigured/unloadable ONNX model)",
                std::env::var(EMBEDDING_BACKEND_ENV).unwrap_or_default()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_a_single_chunk() {
        assert_eq!(
            chunk_text("hello world", 1600, 200, 16),
            vec!["hello world"]
        );
    }

    #[test]
    fn blank_text_yields_no_chunks() {
        assert!(chunk_text("   ", 1600, 200, 16).is_empty());
        assert!(chunk_text("", 1600, 200, 16).is_empty());
    }

    #[test]
    fn long_text_is_split_into_overlapping_windows() {
        let text = "word ".repeat(100); // 500 chars
        let chunks = chunk_text(&text, 100, 20, 16);
        assert!(chunks.len() > 1);
        // Consecutive windows overlap: the tail of one reappears at the head
        // of the next.
        for pair in chunks.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let tail: String = a
                .chars()
                .rev()
                .take(10)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            assert!(
                b.contains(tail.trim()),
                "expected overlap between {:?} and {:?}",
                a,
                b
            );
        }
    }

    #[test]
    fn a_cut_prefers_the_nearest_space_to_avoid_splitting_a_word() {
        let text = "aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd";
        let chunks = chunk_text(text, 15, 2, 16);
        for chunk in &chunks {
            assert!(
                !chunk.split(' ').any(|w| w.len() > 10),
                "a window split a word: {:?}",
                chunk
            );
        }
    }

    #[test]
    fn max_chunks_is_a_hard_cap() {
        let text = "word ".repeat(1000);
        let chunks = chunk_text(&text, 10, 2, 3);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn multi_byte_characters_near_a_cut_point_are_not_split_mid_codepoint() {
        // Each "é" is two UTF-8 bytes; a byte-indexed cut here would panic or
        // produce invalid UTF-8. A char-indexed one must not.
        let text = "é".repeat(50);
        let chunks = chunk_text(&text, 10, 2, 16);
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(chunk.chars().all(|c| c == 'é'));
        }
    }

    #[test]
    fn role_prefixes_match_known_model_families_by_substring() {
        assert_eq!(
            prefix_for("nomic-embed-text", EmbedRole::Query),
            "search_query: "
        );
        assert_eq!(
            prefix_for("nomic-embed-text", EmbedRole::Passage),
            "search_document: "
        );
        assert_eq!(
            prefix_for("intfloat/e5-base-v2", EmbedRole::Query),
            "query: "
        );
        assert_eq!(
            prefix_for("BAAI/bge-large-en-v1.5", EmbedRole::Passage),
            "",
            "BGE's convention only instructs the query side"
        );
    }

    #[test]
    fn an_unknown_model_gets_no_prefix() {
        assert_eq!(
            prefix_for("sentence-transformers/all-MiniLM-L6-v2", EmbedRole::Query),
            ""
        );
    }

    #[test]
    fn resolve_embedder_is_none_without_the_backend_env_var() {
        // Deliberately does not touch REMIND_ME_EMBEDDING_BACKEND: the
        // process-global env var tests live in embedder_test.rs, serialized
        // behind a lock, so this unit test only asserts the case that is
        // always safe to assume — nobody else in this crate's own test suite
        // sets that variable to "ollama" and leaves it set.
        crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
        assert!(resolve_embedder().is_none());
    }
}
