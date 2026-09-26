//! Vector storage and brute-force semantic search.
//!
//! See `docs/adr/0002-embeddings-ollama-and-brute-force-vectors.md` for why
//! this is a plain table and a Rust-side cosine scan rather than
//! `sqlite-vec`'s `vec0` virtual table: no native extension this crate can
//! load, and a database shared with `remind_me` is unaffected either way —
//! neither side reads the other's vector store.
//!
//! # Storage
//!
//! `vec_chunks` holds one row per chunk, keyed `(memory_id, chunk_ix)`, with
//! the vector itself (schema v30, ADR-0023 §4). Up to v29 the chunks were
//! keyed on `memories.rowid` and the bytes lived in a second table;
//! [`crate::db::migrations`] moves an older database over. The statements
//! live in [`crate::db::vectors`].
//!
//! Vectors are raw little-endian float32 bytes, dimension inferred from
//! `len(bytes) / 4` — matching the reference's own convention, which is what
//! keeps the column backend-agnostic across a 384/768/1024-dimensional model
//! without a schema change.

use crate::db::memories::Memories;
use crate::db::vectors::{ChunkVector, Unembedded, Vectors};
use crate::embedder::{
    chunk_text, EmbedError, EmbedRole, Embedder, EmbeddingIdentity, EMBED_CHUNK_CHARS,
    EMBED_CHUNK_OVERLAP, EMBED_MAX_CHUNKS,
};
use crate::models::Memory;
use rusqlite::{Connection, Result as SqlResult};

/// Why an embedding-touching operation could not complete. Every variant is
/// something a caller can degrade on — search falls back to keyword-only,
/// a write proceeds without its embedding — never a reason to fail the
/// surrounding operation outright.
#[derive(Debug)]
pub enum VectorError {
    Db(rusqlite::Error),
    Embed(EmbedError),
}

impl std::fmt::Display for VectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "{}", e),
            Self::Embed(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for VectorError {}

impl From<rusqlite::Error> for VectorError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

impl From<EmbedError> for VectorError {
    fn from(e: EmbedError) -> Self {
        Self::Embed(e)
    }
}

fn f32_to_le_bytes(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for x in vector {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Dimension of a stored vector, inferred the same way the reference does.
pub fn dimension_of(bytes: &[u8]) -> usize {
    bytes.len() / 4
}

pub(crate) fn le_bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    // A trailing partial chunk (a corrupt blob) is dropped, as it always was.
    let (chunks, _remainder) = bytes.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// Drop every chunk vector belonging to a memory.
///
/// Called from `delete_memory`, so a deleted or tombstoned memory stops
/// matching searches, and from [`embed_and_store`] before writing fresh
/// chunks, so a re-embed replaces rather than accumulates.
pub fn delete_chunks_for_memory(conn: &Connection, memory_id: &str) -> SqlResult<usize> {
    Vectors::new(conn).delete_for(memory_id)
}

/// Store freshly computed chunk vectors for a memory, one per chunk index.
fn store_vectors(conn: &Connection, memory_id: &str, vectors: &[Vec<f32>]) -> SqlResult<usize> {
    let repo = Vectors::new(conn);
    for (chunk_ix, vector) in vectors.iter().enumerate() {
        repo.put(memory_id, chunk_ix, &f32_to_le_bytes(vector))?;
    }
    Ok(vectors.len())
}

/// Chunk, embed, and store one memory's content — replacing whatever chunks
/// it had before.
///
/// Returns the number of chunks stored (`0` for blank content, which
/// [`chunk_text`] already treats as nothing to embed). A memory id that does
/// not resolve to a live row is not an error: it returns `Ok(0)`, since the
/// caller (an add/update path) already knows whether the write it just made
/// succeeded — this only has something to do if it did.
pub fn embed_and_store(
    conn: &Connection,
    embedder: &dyn Embedder,
    memory_id: &str,
    content: &str,
) -> Result<usize, VectorError> {
    if !Memories::new(conn).exists(memory_id)? {
        return Ok(0);
    }

    delete_chunks_for_memory(conn, memory_id)?;

    let chunks = chunk_text(
        content,
        EMBED_CHUNK_CHARS,
        EMBED_CHUNK_OVERLAP,
        EMBED_MAX_CHUNKS,
    );
    if chunks.is_empty() {
        return Ok(0);
    }
    let vectors = embedder.embed(&chunks, EmbedRole::Passage)?;
    let stored = store_vectors(conn, memory_id, &vectors)?;
    if stored > 0 {
        // Best-effort, matching the reference's own
        // `_mark_embedding_meta_current`: this is bookkeeping for the next
        // mismatch check, never a reason to fail a write that already
        // succeeded.
        let _ = mark_embedding_meta_current(conn, &embedder.identity());
    }
    Ok(stored)
}

/// Brute-force cosine-similarity search over every stored chunk vector.
///
/// Embeds `query`, then scans every live, non-superseded memory's chunk
/// vectors, keeping each memory's single best (highest-similarity) chunk —
/// a memory that owns several chunks should not out-rank one that owns one
/// purely for having more shots at matching. Vectors are pre-normalized at
/// embed time, so cosine similarity is the plain dot product.
///
/// Returns memories ordered by similarity, descending, capped at `limit`.
/// Any failure (the embedder rejects the query, a stored vector's dimension
/// no longer matches the query's — e.g. after `REMIND_ME_EMBEDDING_DIM`
/// changed without a reindex) is the caller's to decide how to treat; this
/// never partially-guesses.
pub fn semantic_search(
    conn: &Connection,
    embedder: &dyn Embedder,
    query: &str,
    limit: usize,
    category: Option<&str>,
) -> Result<Vec<Memory>, VectorError> {
    Ok(
        semantic_search_scored(conn, embedder, query, &[], limit, category)?
            .into_iter()
            .map(|(memory, _similarity)| memory)
            .collect(),
    )
}

/// Embed `texts` and average them into one L2-normalised search vector.
///
/// With a single text this is exactly that text's own (already-normalised)
/// embedding, re-normalised — a no-op. With several (e.g. the query plus a
/// HyDE passage from [`crate::query_expansion`]), the mean vector blends
/// question-space and document-space so candidates near either phrasing
/// rank well.
///
/// `texts[0]` is always the literal search query and is embedded with
/// [`EmbedRole::Query`]; any remaining texts are passage-like expansion
/// text, embedded with [`EmbedRole::Passage`] — otherwise a query-prefixed
/// model would apply the wrong instruction to half the fused vector's
/// inputs. Matches the reference's `db._fuse_query_embedding`.
///
/// # Panics
/// Never — `texts` empty returns an empty vector, the same degrade-not-fail
/// contract as everything else here.
pub fn fuse_query_embedding(
    embedder: &dyn Embedder,
    texts: &[String],
) -> Result<Vec<f32>, EmbedError> {
    let Some((query_text, extra_texts)) = texts.split_first() else {
        return Ok(Vec::new());
    };
    let mut vecs = embedder.embed(std::slice::from_ref(query_text), EmbedRole::Query)?;
    if !extra_texts.is_empty() {
        vecs.extend(embedder.embed(extra_texts, EmbedRole::Passage)?);
    }
    let dim = vecs.first().map(|v| v.len()).unwrap_or(0);
    if dim == 0 {
        return Ok(Vec::new());
    }
    let mut fused = vec![0.0f32; dim];
    for v in &vecs {
        for (f, x) in fused.iter_mut().zip(v.iter()) {
            *f += x;
        }
    }
    let n = vecs.len() as f32;
    for f in fused.iter_mut() {
        *f /= n;
    }
    Ok(crate::embedder::l2_normalize(fused))
}

/// Same as [`semantic_search`], but keeps each memory's raw cosine
/// similarity alongside it (highest first) instead of discarding it.
///
/// [`crate::retrieval::rank_rrf`]'s `"score"` fusion mode needs the actual
/// match *magnitude*, not just list position, to normalize against — this is
/// that magnitude's only source, since nothing else in this crate computes
/// it.
pub fn semantic_search_scored(
    conn: &Connection,
    embedder: &dyn Embedder,
    query: &str,
    extra_texts: &[String],
    limit: usize,
    category: Option<&str>,
) -> Result<Vec<(Memory, f32)>, VectorError> {
    let mut fuse_texts = Vec::with_capacity(1 + extra_texts.len());
    fuse_texts.push(query.to_string());
    fuse_texts.extend(extra_texts.iter().cloned());
    let query_vector = fuse_query_embedding(embedder, &fuse_texts)?;
    if query_vector.is_empty() {
        return Ok(Vec::new());
    }

    // The index, when usable, narrows which rows are scanned. It never scores:
    // the exact dot product runs over whatever survives, so results are
    // identical either way and nothing downstream needs to know which path
    // ran. `None` means scan everything — a search must not fail, or change
    // its answers, because an optimisation was unavailable.
    if let Some(narrowed) = crate::ann_index::candidates(conn, &query_vector, limit) {
        let scored = scan_and_score(conn, &query_vector, limit, category, Some(&narrowed))?;
        // A category filter can remove most of what the index proposed.
        // Returning fewer results than a full scan would have is a retrieval
        // regression nobody would notice, so fall back rather than accept a
        // short list.
        if scored.len() >= limit {
            return Ok(scored);
        }
    }
    scan_and_score(conn, &query_vector, limit, category, None)
}

/// Score every candidate exactly and return the best `limit`.
///
/// `narrowed` restricts which memory ids are considered; `None` scans all
/// of them. Scoring is identical in both cases — that is the whole point of
/// letting the index propose candidates rather than rank them.
fn scan_and_score(
    conn: &Connection,
    query_vector: &[f32],
    limit: usize,
    category: Option<&str>,
    narrowed: Option<&[String]>,
) -> Result<Vec<(Memory, f32)>, VectorError> {
    // The same filter the keyword branch of search applies, so the two ranked
    // lists retrieval fuses answer the same question.
    let category = category.filter(|c| !c.is_empty());
    let chunks = Vectors::new(conn).live_chunks(category, narrowed)?;

    let mut best_by_memory: std::collections::HashMap<String, f32> =
        std::collections::HashMap::new();
    for ChunkVector {
        memory_id,
        embedding: bytes,
    } in chunks
    {
        let vector = le_bytes_to_f32(&bytes);
        if vector.len() != query_vector.len() {
            // A stale vector from a dimension this store no longer embeds
            // at — skip it rather than let a mismatched dot product either
            // panic or silently misrank.
            continue;
        }
        let similarity: f32 = query_vector
            .iter()
            .zip(vector.iter())
            .map(|(a, b)| a * b)
            .sum();
        best_by_memory
            .entry(memory_id)
            .and_modify(|best| {
                if similarity > *best {
                    *best = similarity;
                }
            })
            .or_insert(similarity);
    }

    let mut ranked: Vec<(String, f32)> = best_by_memory.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(limit);

    let mut memories = Vec::with_capacity(ranked.len());
    for (memory_id, similarity) in ranked {
        if let Some(memory) = crate::db::queries::get_memory_by_id(conn, &memory_id)? {
            memories.push((memory, similarity));
        }
    }
    Ok(memories)
}

// ---------------------------------------------------------------------------
// Reindex
// ---------------------------------------------------------------------------

/// Every live memory with no `vec_chunks` row — no cap, matching the
/// reference's own `remind_me_reindex`, which takes no inputs and processes
/// everything missing in one call. [`crate::embedder::EMBED_FORWARD_BATCH`]
/// is what actually bounds request size, per HTTP call to the embedder, the
/// same way the reference bounds its ONNX forward pass — this is not a
/// second, redundant limit on top of that.
fn unembedded(conn: &Connection) -> SqlResult<Vec<Unembedded>> {
    Vectors::new(conn).unembedded()
}

/// What one `remind_me_reindex` call did.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReindexResult {
    /// Memories with no `vec_chunks` row before this call.
    pub missing: usize,
    /// Of those, how many now have at least one chunk vector.
    pub embedded: usize,
    pub chunks_created: usize,
    /// No embedder configured or reachable — nothing could be done.
    pub degraded: bool,
}

/// Embed every memory that has never been embedded. Existing embeddings are
/// untouched — this is additive, not a rebuild — which is what makes it safe
/// to run repeatedly: it is the documented recovery path after an
/// export/import round-trip (embeddings are deliberately not part of an
/// export, since they are derived data) and after a bulk import (`dbs`,
/// MemPalace, chat/document) that never had an embedder wired into it.
///
/// No inputs, matching the reference's own `remind_me_reindex` exactly, and
/// no artificial per-call cap on top of it either: one memory's embedding
/// failure (the daemon dropped mid-batch, say) does not abort the rest —
/// this simply continues, and that memory stays missing for the next call
/// to pick back up, the same as any other still-unembedded memory.
pub fn reindex(conn: &Connection) -> Result<ReindexResult, VectorError> {
    let Some(embedder) = crate::embedder::available_embedder() else {
        return Ok(ReindexResult {
            degraded: true,
            ..Default::default()
        });
    };
    reindex_with(conn, &*embedder)
}

/// Same as [`reindex`], but takes the embedder explicitly instead of
/// resolving it from the environment — this is what makes the embed-the-
/// missing-ones behavior testable with a deterministic fake, since
/// `reindex` itself always goes through the env-configured, TTL-cached
/// singleton.
pub fn reindex_with(
    conn: &Connection,
    embedder: &dyn Embedder,
) -> Result<ReindexResult, VectorError> {
    let missing = unembedded(conn)?;
    let mut result = ReindexResult {
        missing: missing.len(),
        ..Default::default()
    };

    for memory in missing {
        if let Ok(chunks) = embed_and_store(conn, embedder, &memory.id, &memory.content) {
            if chunks > 0 {
                result.embedded += 1;
                result.chunks_created += chunks;
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Embedding-model versioning (#96)
// ---------------------------------------------------------------------------
//
// The reference (`67570ce`) records which model/dimension/backend produced
// `memories_vec`'s vectors in an `embedding_meta` table, checks it against
// the configured model at every startup (`_reconcile_embedding_meta`), and
// on a mismatch clears `memories_vec`/`vec_chunks` (recreating `memories_vec`
// at the new dimension, since `vec0`'s column type bakes the dimension in)
// plus its on-disk ANN index, so every memory falls back to the existing
// "missing embeddings" path instead of silently serving results computed
// against the wrong embedding space.
//
// This crate's `vec_chunks.embedding` (ADR-0002) is a plain `BLOB` column,
// not a `vec0` virtual table, so a dimension change needs no `DROP`/`CREATE`:
// clearing rows is the whole story. The optional ANN index
// (`crate::ann_index`) needs no invalidating either, because it records the
// chunk count and dimension it was built from and ignores itself once they
// change. See ADR-0002's addendum for the full adaptation writeup.

/// Read the model/dimension/backend recorded for the vectors currently in
/// `vec_chunks`, if any.
///
/// `None` covers both "never recorded" (a fresh store, or one written before
/// this feature existed) and a partially-written record (only some of the
/// three keys present) — either way there is nothing complete to compare
/// against, so callers must treat this the same as "nothing recorded" rather
/// than guess at the missing piece.
fn read_embedding_meta(conn: &Connection) -> SqlResult<Option<EmbeddingIdentity>> {
    let rows = Vectors::new(conn).meta()?;
    if rows.is_empty() {
        return Ok(None);
    }
    let stored: std::collections::HashMap<String, String> = rows.into_iter().collect();
    let (Some(backend), Some(model), Some(dim)) = (
        stored.get("backend"),
        stored.get("model"),
        stored.get("dim"),
    ) else {
        return Ok(None);
    };
    let Ok(dim) = dim.parse::<usize>() else {
        return Ok(None);
    };
    Ok(Some(EmbeddingIdentity {
        backend: backend.clone(),
        model: model.clone(),
        dim,
    }))
}

/// Record that the vectors in `vec_chunks` were (just) produced by
/// `identity` — called from [`embed_and_store`] after a batch of vectors is
/// successfully written, not merely inferred from the running config, so the
/// mismatch check below stays accurate even mid-reindex (a reindex that dies
/// partway through has already marked every memory it did finish as
/// current).
pub fn mark_embedding_meta_current(
    conn: &Connection,
    identity: &EmbeddingIdentity,
) -> SqlResult<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let vectors = Vectors::new(conn);
    for (key, value) in [
        ("backend", identity.backend.clone()),
        ("model", identity.model.clone()),
        ("dim", identity.dim.to_string()),
    ] {
        vectors.set_meta(key, &value, &now)?;
    }
    Ok(())
}

/// What changed, when a stored/current embedding-identity mismatch is found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingMismatch {
    pub stored: EmbeddingIdentity,
    pub current: EmbeddingIdentity,
}

/// Read-only check: does the model/dimension/backend recorded for the
/// currently-stored vectors differ from `current`?
///
/// Returns `None` when nothing is recorded yet (see [`read_embedding_meta`])
/// or when the recorded identity matches `current` — in both cases there is
/// nothing to clear. This is what keeps a first-ever run (nothing recorded)
/// from being treated as a mismatch: there is no "old" model to have
/// changed away from.
pub fn embedding_mismatch_info(
    conn: &Connection,
    current: &EmbeddingIdentity,
) -> SqlResult<Option<EmbeddingMismatch>> {
    let Some(stored) = read_embedding_meta(conn)? else {
        return Ok(None);
    };
    if &stored == current {
        return Ok(None);
    }
    Ok(Some(EmbeddingMismatch {
        stored,
        current: current.clone(),
    }))
}

/// Clear stale vectors when the embedding model/dimension/backend recorded
/// for them no longer matches `current` — the reference's auto-clear
/// (`_reconcile_embedding_meta`), adapted to this crate's own `vec_chunks`
/// table (see the note above for why no table recreation or ANN
/// invalidation is needed here).
///
/// Deliberately does **not** update `embedding_meta` itself: that only
/// happens once vectors are actually rewritten
/// ([`mark_embedding_meta_current`], called from [`embed_and_store`]), so the
/// mismatch stays flagged until a real reindex happens, not just until the
/// next call to this function.
///
/// Called from [`crate::db::schema::initialize_schema`] on every open, the
/// same "check at startup" timing the reference uses. A no-op both when
/// nothing is recorded yet (first-ever run) and when the recorded identity
/// already matches `current`.
pub fn reconcile_embedding_meta(
    conn: &Connection,
    current: &EmbeddingIdentity,
) -> SqlResult<Option<EmbeddingMismatch>> {
    let Some(mismatch) = embedding_mismatch_info(conn, current)? else {
        return Ok(None);
    };
    Vectors::new(conn).clear()?;
    Ok(Some(mismatch))
}
