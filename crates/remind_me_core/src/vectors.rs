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
//! [`vec_chunks`][crate::db] (already in the generated schema) is only the
//! rowid map back to `memory_rowid`/`chunk_ix` — that is the reference's own
//! separation, and it stays untouched: `schema_tables.sql` is generated
//! verbatim and is not this crate's file to extend. The actual bytes live in
//! [`ensure_schema`]'s new `vec_embeddings` table, joined to `vec_chunks` by
//! `vec_rowid`.
//!
//! Vectors are raw little-endian float32 bytes, dimension inferred from
//! `len(bytes) / 4` — matching the reference's own convention, which is what
//! keeps the column backend-agnostic across a 384/768/1024-dimensional model
//! without a schema change.

use crate::db::queries::{parse_memory_row, MEMORY_COLUMNS};
use crate::embedder::{
    chunk_text, EmbedError, EmbedRole, Embedder, EmbeddingIdentity, EMBED_CHUNK_CHARS,
    EMBED_CHUNK_OVERLAP, EMBED_MAX_CHUNKS,
};
use crate::models::Memory;
use rusqlite::{params, params_from_iter, Connection, Result as SqlResult};

/// Create this crate's own vector table, if it does not already exist.
///
/// Called from [`crate::db::schema::initialize_schema`], after the generated
/// schema is applied — the same arrangement as [`crate::sync::prune_outbox`]:
/// a step this crate adds on top of, not part of, the generated tables.
pub fn ensure_schema(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vec_embeddings (
            vec_rowid INTEGER PRIMARY KEY REFERENCES vec_chunks(vec_rowid),
            embedding BLOB NOT NULL
        );",
    )
}

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
/// SQLite reuses freed rowids: without this, a later memory that happens to
/// land on the same `rowid` would silently inherit the deleted memory's
/// embeddings through the surviving `vec_chunks` rows. Called from
/// `delete_memory` for exactly that reason, and from [`embed_and_store`]
/// before writing fresh chunks so a re-embed replaces rather than
/// accumulates.
pub fn delete_chunks_for_memory(conn: &Connection, memory_rowid: i64) -> SqlResult<usize> {
    let vec_rowids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT vec_rowid FROM vec_chunks WHERE memory_rowid = ?")?;
        let rows = stmt.query_map(params![memory_rowid], |r| r.get(0))?;
        rows.collect::<SqlResult<_>>()?
    };
    for vec_rowid in &vec_rowids {
        conn.execute(
            "DELETE FROM vec_embeddings WHERE vec_rowid = ?",
            params![vec_rowid],
        )?;
    }
    conn.execute(
        "DELETE FROM vec_chunks WHERE memory_rowid = ?",
        params![memory_rowid],
    )?;
    Ok(vec_rowids.len())
}

/// Store freshly computed chunk vectors for a memory.
///
/// Does not clear any existing chunks first — callers that are re-embedding
/// (as opposed to embedding for the first time) call
/// [`delete_chunks_for_memory`] themselves, so a caller that only ever wants
/// to append (there is no such caller today) is not forced to pay a delete
/// it doesn't need.
fn store_vectors(conn: &Connection, memory_rowid: i64, vectors: &[Vec<f32>]) -> SqlResult<usize> {
    for (chunk_ix, vector) in vectors.iter().enumerate() {
        conn.execute(
            "INSERT INTO vec_chunks (memory_rowid, chunk_ix) VALUES (?, ?)",
            params![memory_rowid, chunk_ix as i64],
        )?;
        let vec_rowid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO vec_embeddings (vec_rowid, embedding) VALUES (?, ?)",
            params![vec_rowid, f32_to_le_bytes(vector)],
        )?;
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
    let memory_rowid: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM memories WHERE id = ?",
            params![memory_id],
            |r| r.get(0),
        )
        .ok();
    let Some(memory_rowid) = memory_rowid else {
        return Ok(0);
    };

    delete_chunks_for_memory(conn, memory_rowid)?;

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
    let stored = store_vectors(conn, memory_rowid, &vectors)?;
    if stored > 0 {
        // Best-effort, matching the reference's own
        // `_mark_embedding_meta_current`: this is bookkeeping for the next
        // mismatch check, never a reason to fail a write that already
        // succeeded.
        let _ = mark_embedding_meta_current(conn, &embedder.identity());
    }
    Ok(stored)
}

fn get_memory_by_rowid(conn: &Connection, rowid: i64) -> SqlResult<Option<Memory>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE rowid = ?",
        MEMORY_COLUMNS
    ))?;
    let mut rows = stmt.query_map(params![rowid], parse_memory_row)?;
    rows.next().transpose()
}

/// Category condition shared by the semantic scan — kept to the same filter
/// [`crate::db::queries::search_memories`]'s keyword branch applies, so the
/// two ranked lists [`crate::retrieval::rank_rrf`] fuses are answering the
/// same question.
fn category_filter(category: Option<&str>) -> (String, Vec<String>) {
    match category {
        Some(cat) if !cat.is_empty() => (" AND m.category = ?".to_string(), vec![cat.to_string()]),
        _ => (String::new(), Vec::new()),
    }
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
/// `narrowed` restricts which memory rowids are considered; `None` scans all
/// of them. Scoring is identical in both cases — that is the whole point of
/// letting the index propose candidates rather than rank them.
fn scan_and_score(
    conn: &Connection,
    query_vector: &[f32],
    limit: usize,
    category: Option<&str>,
    narrowed: Option<&[i64]>,
) -> Result<Vec<(Memory, f32)>, VectorError> {
    let (filter_sql, filter_bindings) = category_filter(category);
    let narrow_sql = match narrowed {
        Some(rowids) => format!(
            " AND vc.memory_rowid IN ({})",
            rowids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        None => String::new(),
    };
    let sql = format!(
        "SELECT vc.memory_rowid, ve.embedding
           FROM vec_chunks vc
           JOIN vec_embeddings ve ON ve.vec_rowid = vc.vec_rowid
           JOIN memories m ON m.rowid = vc.memory_rowid
          WHERE m.superseded_by IS NULL AND m.deleted_at IS NULL{}{}",
        filter_sql, narrow_sql
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(filter_bindings.iter()), |row| {
        let memory_rowid: i64 = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        Ok((memory_rowid, bytes))
    })?;

    let mut best_by_memory: std::collections::HashMap<i64, f32> = std::collections::HashMap::new();
    for row in rows {
        let (memory_rowid, bytes) = row?;
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
            .entry(memory_rowid)
            .and_modify(|best| {
                if similarity > *best {
                    *best = similarity;
                }
            })
            .or_insert(similarity);
    }

    let mut ranked: Vec<(i64, f32)> = best_by_memory.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(limit);

    let mut memories = Vec::with_capacity(ranked.len());
    for (rowid, similarity) in ranked {
        if let Some(memory) = get_memory_by_rowid(conn, rowid)? {
            memories.push((memory, similarity));
        }
    }
    Ok(memories)
}

// ---------------------------------------------------------------------------
// Reindex
// ---------------------------------------------------------------------------

/// A memory with no `vec_chunks` row at all — never embedded.
struct Unembedded {
    id: String,
    content: String,
}

/// Every live memory with no `vec_chunks` row — no cap, matching the
/// reference's own `remind_me_reindex`, which takes no inputs and processes
/// everything missing in one call. [`crate::embedder::EMBED_FORWARD_BATCH`]
/// is what actually bounds request size, per HTTP call to the embedder, the
/// same way the reference bounds its ONNX forward pass — this is not a
/// second, redundant limit on top of that.
fn unembedded(conn: &Connection) -> SqlResult<Vec<Unembedded>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.content
           FROM memories m
          WHERE m.deleted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM vec_chunks vc WHERE vc.memory_rowid = m.rowid)
          ORDER BY m.rowid",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Unembedded {
            id: row.get(0)?,
            content: row.get(1)?,
        })
    })?;
    rows.collect()
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
// This crate's own `vec_embeddings` (ADR-0002) is a plain `BLOB` column, not
// a `vec0` virtual table, so a dimension change needs no `DROP`/`CREATE` —
// clearing rows is the whole story. There is also no on-disk ANN index here
// (ADR-0002 scopes that out entirely): brute-force cosine scan is the only
// search path, so nothing beyond `vec_embeddings`/`vec_chunks` needs
// invalidating. See ADR-0002's addendum for the full adaptation writeup.

/// Read the model/dimension/backend recorded for the vectors currently in
/// `vec_embeddings`, if any.
///
/// `None` covers both "never recorded" (a fresh store, or one written before
/// this feature existed) and a partially-written record (only some of the
/// three keys present) — either way there is nothing complete to compare
/// against, so callers must treat this the same as "nothing recorded" rather
/// than guess at the missing piece.
fn read_embedding_meta(conn: &Connection) -> SqlResult<Option<EmbeddingIdentity>> {
    let mut stmt = conn.prepare("SELECT key, value FROM embedding_meta")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<SqlResult<_>>()?;
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

/// Record that the vectors in `vec_embeddings` were (just) produced by
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
    for (key, value) in [
        ("backend", identity.backend.clone()),
        ("model", identity.model.clone()),
        ("dim", identity.dim.to_string()),
    ] {
        conn.execute(
            "INSERT INTO embedding_meta (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now],
        )?;
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
/// (`_reconcile_embedding_meta`), adapted to this crate's own
/// `vec_embeddings` table (see the module-level note above for why no table
/// recreation or ANN invalidation is needed here).
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
    // Child before parent: `vec_embeddings.vec_rowid` has a (default
    // RESTRICT) foreign key onto `vec_chunks.vec_rowid`, so clearing
    // `vec_chunks` first would fail with `foreign_keys=ON` while
    // `vec_embeddings` rows still reference it.
    conn.execute("DELETE FROM vec_embeddings", [])?;
    conn.execute("DELETE FROM vec_chunks", [])?;
    Ok(Some(mismatch))
}
