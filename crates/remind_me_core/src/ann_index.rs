//! Approximate nearest-neighbour index over the stored chunk embeddings.
//!
//! # This is a scale improvement, never a source of truth
//!
//! Brute-force search already works and already returns exact answers. The
//! index exists only to stop vector recall being a full scan of every chunk.
//! So every failure mode here — no index, a stale one, an unreadable one, a
//! dimension that no longer matches — resolves to **fall back to brute force**
//! rather than to an error or a wrong answer. A search must never fail because
//! an optimisation was unavailable.
//!
//! # The index narrows candidates; it does not score them
//!
//! ANN returns approximate neighbours. Rather than trusting its distances,
//! this over-fetches candidates and hands the caller their rowids, and the
//! caller then computes the **exact** dot product over that much smaller set.
//! Two things fall out, both wanted:
//!
//! - scores are identical to the brute-force ones, so nothing downstream
//!   (RRF fusion in particular) has to know whether the index was used;
//! - a category filter can be applied during exact scoring, which the index
//!   itself cannot express.
//!
//! Over-fetching is what makes the filter safe: after filtering, if fewer than
//! `limit` candidates survive, the caller falls back rather than returning a
//! short list. An ANN path that silently returns fewer results than brute
//! force would be a retrieval regression nobody would notice.
//!
//! # Staleness is detected, not assumed away
//!
//! The index records how many vectors it was built from. If the live count
//! differs, the index is stale and is ignored. A stale index quietly returning
//! deleted memories is the specific failure this guards against — it is worse
//! than no index at all, because the results look plausible.
//!
//! Building is always explicit. A search must not silently pay for an index
//! build; that would turn one slow query into a pathologically slow one at
//! exactly the moment someone is waiting.

use rusqlite::{Connection, Result as SqlResult};

/// How many extra candidates to pull before filtering and exact scoring.
///
/// Enough that a category filter removing most hits still leaves a full page,
/// small enough that exact scoring stays cheap.
pub const OVERFETCH: usize = 8;

/// Whether this build has an index at all.
pub fn available() -> bool {
    cfg!(feature = "ann")
}

/// Where the index for a given database lives.
pub fn index_path(conn: &Connection) -> Option<std::path::PathBuf> {
    let db: String = conn
        .query_row("PRAGMA database_list", [], |r| r.get::<_, String>(2))
        .ok()?;
    if db.is_empty() {
        return None; // in-memory: nowhere to persist, so no index
    }
    Some(std::path::PathBuf::from(format!("{}.ann", db)))
}

/// How many embeddings the store currently holds, and at what dimension.
///
/// The pair is the staleness key: either changing means an index built from
/// the old state cannot be trusted.
pub fn live_signature(conn: &Connection) -> SqlResult<(usize, usize)> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM vec_embeddings", [], |r| r.get(0))?;
    let dimension: usize = conn
        .query_row("SELECT embedding FROM vec_embeddings LIMIT 1", [], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .map(|bytes| crate::vectors::dimension_of(&bytes))
        .unwrap_or(0);
    Ok((count as usize, dimension))
}

#[cfg(feature = "ann")]
mod backend {
    use super::*;
    use usearch::ffi::{IndexOptions, MetricKind, ScalarKind};

    fn options(dimensions: usize) -> IndexOptions {
        IndexOptions {
            dimensions,
            // Cosine, matching what the brute-force path's dot product means
            // for the normalised vectors this store writes. A different metric
            // here would make the candidate set disagree with the scoring.
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            ..Default::default()
        }
    }

    /// Build the index from every stored embedding and persist it.
    ///
    /// Explicit: nothing calls this from a search path.
    pub fn build(conn: &Connection) -> Result<usize, String> {
        let Some(path) = index_path(conn) else {
            return Err(
                "this database is in-memory, so there is nowhere to persist an index".into(),
            );
        };
        let (count, dimension) = live_signature(conn).map_err(|e| e.to_string())?;
        if count == 0 || dimension == 0 {
            return Err("no embeddings to index yet — run a reindex first".into());
        }

        let index = usearch::new_index(&options(dimension)).map_err(|e| e.to_string())?;
        index.reserve(count).map_err(|e| e.to_string())?;

        let mut stmt = conn
            .prepare(
                "SELECT vc.memory_rowid, ve.embedding
                        FROM vec_chunks vc
                        JOIN vec_embeddings ve ON ve.vec_rowid = vc.vec_rowid",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
            .map_err(|e| e.to_string())?;

        let mut added = 0usize;
        // Persisted alongside the index, not held in memory. An in-process map
        // would make the index work only in the process that built it and fall
        // back silently everywhere else — a feature that looks enabled and
        // never actually runs.
        let mut keys: Vec<i64> = Vec::with_capacity(count);
        for (position, row) in rows.enumerate() {
            let (memory_rowid, bytes) = row.map_err(|e| e.to_string())?;
            let vector = crate::vectors::le_bytes_to_f32(&bytes);
            if vector.len() != dimension {
                // A stale vector from a dimension this store no longer embeds
                // at. Skipped rather than allowed to poison the index.
                continue;
            }
            // Keyed by position, not by memory rowid: several chunks share one
            // memory, and a keyed-by-memory index would silently keep only the
            // last chunk of each.
            index
                .add(position as u64, &vector)
                .map_err(|e| e.to_string())?;
            keys.push(memory_rowid);
            added += 1;
        }

        index
            .save(&path.to_string_lossy())
            .map_err(|e| e.to_string())?;
        let manifest = std::iter::once(format!("{} {}", count, dimension))
            .chain(keys.iter().map(|id| id.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(sidecar(&path), manifest).map_err(|e| e.to_string())?;
        Ok(added)
    }

    fn sidecar(path: &std::path::Path) -> std::path::PathBuf {
        path.with_extension("ann.meta")
    }

    /// Candidate memory rowids for a query, or `None` to fall back.
    pub fn candidates(conn: &Connection, query: &[f32], want: usize) -> Option<Vec<i64>> {
        let path = index_path(conn)?;
        if !path.exists() {
            return None;
        }

        let (live_count, live_dimension) = live_signature(conn).ok()?;
        let recorded = std::fs::read_to_string(sidecar(&path)).ok()?;
        let mut lines = recorded.lines();
        let mut header = lines.next()?.split_whitespace();
        let built_count: usize = header.next()?.parse().ok()?;
        let built_dimension: usize = header.next()?.parse().ok()?;
        // Position → memory rowid, in the order they were added.
        let keys: Vec<i64> = lines.filter_map(|l| l.trim().parse().ok()).collect();

        // Stale, or built at a dimension this query is not in. Either way the
        // answers would be wrong in a way that still looks plausible.
        if built_count != live_count || built_dimension != live_dimension {
            return None;
        }
        if query.len() != live_dimension {
            return None;
        }

        let index = usearch::new_index(&options(built_dimension)).ok()?;
        index.load(&path.to_string_lossy()).ok()?;

        let matches = index
            .search(query, want.saturating_mul(OVERFETCH).max(want))
            .ok()?;
        let mut rowids: Vec<i64> = Vec::with_capacity(matches.keys.len());
        for key in matches.keys {
            // Several chunks map to one memory, so the same rowid can come
            // back more than once — deduplicated here rather than left for the
            // SQL `IN` list to absorb.
            if let Some(rowid) = keys.get(key as usize) {
                if !rowids.contains(rowid) {
                    rowids.push(*rowid);
                }
            }
        }
        // An empty candidate set is indistinguishable from "the index is not
        // usable", and falling back costs one scan rather than silently
        // returning nothing.
        if rowids.is_empty() {
            return None;
        }
        Some(rowids)
    }
}

#[cfg(not(feature = "ann"))]
mod backend {
    use super::*;

    pub fn build(_conn: &Connection) -> Result<usize, String> {
        Err(
            "ANN indexing is not available in this build: rebuild with the \
             `ann` feature (cargo build --features ann)."
                .into(),
        )
    }

    pub fn candidates(_conn: &Connection, _query: &[f32], _want: usize) -> Option<Vec<i64>> {
        None
    }
}

/// Build and persist the index. Explicit; never called from a search.
pub fn build(conn: &Connection) -> Result<usize, String> {
    backend::build(conn)
}

/// Candidate memory rowids to score exactly, or `None` when the caller should
/// fall back to a full scan.
pub fn candidates(conn: &Connection, query: &[f32], want: usize) -> Option<Vec<i64>> {
    backend::candidates(conn, query, want)
}
