//! Search expansions: three ways to surface memories adjacent to a result set.
//!
//! | flag | source | what it finds |
//! |---|---|---|
//! | `expand_entities` | `memory_entities` | other memories mentioning the same entities |
//! | `include_neighbors` | `doc_id` / `chunk_index` | sibling chunks of the same document |
//! | `expand_co_retrieval` | `memory_associations` | memories retrieved alongside these before |
//!
//! # These sit outside the ranking
//!
//! Every expansion is returned in its own section, capped at
//! [`EXPANSION_CAP`] items with [`SNIPPET_CHARS`]-character snippets, and is
//! **never merged into the ranked results**. So they do not compete with
//! `limit`, and the caps plus the snippet length are what bound their cost —
//! they sit outside the token-budget envelope.
//!
//! For co-retrieval the one-way flow is the entire point: search results →
//! recorded associations → surfaced as *suggestions*, never as a ranking
//! input. Letting a recorded weight reach the ranking would build a feedback
//! loop where whatever was returned together once is returned together
//! forever. Keeping it out means no decay maths is needed to counteract one.
//!
//! Expanded hits are also deliberately **not** access-recorded: they are a
//! discovery aid surfaced by adjacency, not direct matches for the query, and
//! recording them would inflate the vitality of every neighbour on every
//! expanded search.

use crate::models::MemorySearchResult;
use chrono::Utc;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Result};
use serde::{Deserialize, Serialize};

/// Maximum items any one expansion returns.
pub const EXPANSION_CAP: usize = 5;
/// Characters of content carried by an expansion item.
pub const SNIPPET_CHARS: usize = 300;
/// Chunk positions either side of a seed that count as neighbours.
pub const NEIGHBOR_WINDOW: i64 = 1;
/// How many of a result set's memories participate in co-retrieval pairing.
///
/// Pairing is quadratic, so this bounds the writes one search can produce at
/// `10 * 9 / 2 = 45`.
pub const CO_RETRIEVAL_PAIR_CAP: usize = 10;
/// Ceiling on an association's weight, so a pair retrieved together forever
/// does not run away.
pub const CO_RETRIEVAL_MAX_WEIGHT: i64 = 50;

/// One memory surfaced by an expansion, with the reason it was surfaced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedMemory {
    pub id: String,
    pub content_snippet: String,
    pub category: String,
    pub created_at: String,
    /// Names of the entities linking this to a seed. `expand_entities` only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub via_entities: Vec<String>,
    /// Source document and position. `include_neighbors` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_index: Option<i64>,
    /// How often this was retrieved alongside a seed. `expand_co_retrieval` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub co_retrieval_weight: Option<i64>,
}

impl RelatedMemory {
    fn new(id: String, content: &str, category: String, created_at: String) -> Self {
        Self {
            id,
            // By characters, not bytes: a multi-byte character straddling the
            // boundary would panic a byte slice.
            content_snippet: content.chars().take(SNIPPET_CHARS).collect(),
            category,
            created_at,
            via_entities: Vec::new(),
            doc_id: None,
            chunk_index: None,
            co_retrieval_weight: None,
        }
    }
}

/// A search result set plus whichever expansions were requested.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResponse {
    pub memories: Vec<MemorySearchResult>,
    // The token-budget envelope (#200). The trimming already happened; before
    // this, nothing said so, and a response cut in half was indistinguishable
    // from a complete one.
    /// How many candidates were ranked before the budget was applied.
    pub total_candidates: usize,
    /// How many survived it.
    pub returned: usize,
    /// How many were dropped — a count, as in the reference.
    pub trimmed: usize,
    /// Estimated tokens across `memories`.
    pub tokens_used: usize,
    /// The budget that was in force. `0` means unlimited.
    pub budget: usize,
    /// What the wall-clock deadline did to this search (#257).
    ///
    /// Always present, unlike the expansions: "no deadline, nothing skipped"
    /// is a fact worth stating on every response, and a caller checking
    /// `degraded()` should not have to distinguish absent from clean.
    #[serde(default)]
    pub timing: crate::retrieval::SearchTiming,
    /// The L2/L3 persona bootstrap, when one was asked for (#255).
    ///
    /// `None` and `Some(empty)` are deliberately different answers: the first
    /// means the caller did not ask, the second that they asked and the
    /// persona had nothing to give. Collapsing them would make "the ladder is
    /// not producing anything" look identical to "the bootstrap is switched
    /// off", which is the confusion this field exists to end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<crate::promotion::Bootstrap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_via_entities: Option<Vec<RelatedMemory>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_via_neighbors: Option<Vec<RelatedMemory>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_via_co_retrieval: Option<Vec<RelatedMemory>>,
}

/// One sibling-chunk row, named rather than a tuple so the column order cannot
/// be silently transposed.
struct NeighborRow {
    id: String,
    content: String,
    category: String,
    created_at: String,
    doc_id: Option<String>,
    chunk_index: Option<i64>,
}

fn text_params(ids: &[String]) -> Vec<Value> {
    ids.iter().map(|id| Value::Text(id.clone())).collect()
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Reinforce the association between every pair of co-retrieved memories.
///
/// Called on **every** search that returns two or more results, whether or not
/// the caller asked to see associations — surfacing is opt-in, recording is
/// not. A graph that only filled when someone was already looking at it would
/// never have anything to show.
///
/// Pairs are **sorted before insert**, so `(a, b)` and `(b, a)` are one row.
/// Without that every weight would be split across two rows and read back at
/// half strength. The composite primary key does not impose an order, so this
/// is the write path's responsibility.
///
/// Returns the number of pairs touched.
pub fn record_co_retrieval(conn: &Connection, memory_ids: &[String]) -> Result<usize> {
    let ids = &memory_ids[..memory_ids.len().min(CO_RETRIEVAL_PAIR_CAP)];
    if ids.len() < 2 {
        return Ok(0);
    }

    let now = Utc::now().to_rfc3339();
    let mut touched = 0;
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            let (a, b) = if ids[i] <= ids[j] {
                (&ids[i], &ids[j])
            } else {
                (&ids[j], &ids[i])
            };
            conn.execute(
                "INSERT INTO memory_associations (memory_id_a, memory_id_b, weight, updated_at)
                 VALUES (?, ?, 1, ?)
                 ON CONFLICT(memory_id_a, memory_id_b) DO UPDATE SET
                     weight = MIN(weight + 1, ?),
                     updated_at = excluded.updated_at",
                params![a, b, now, CO_RETRIEVAL_MAX_WEIGHT],
            )?;
            touched += 1;
        }
    }
    Ok(touched)
}

/// Other memories mentioning the same entities as the seeds, newest first.
///
/// Inner joins throughout, so a link whose endpoints have not arrived — sync
/// can deliver them out of order — stays invisible rather than producing a row
/// with holes in it.
pub fn expand_via_entities(conn: &Connection, seed_ids: &[String]) -> Result<Vec<RelatedMemory>> {
    if seed_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ph = placeholders(seed_ids.len());
    let sql = format!(
        "SELECT m.id, m.content, m.category, m.created_at, e.name AS entity_name
           FROM memory_entities seed
           JOIN memory_entities nbr ON nbr.entity_id = seed.entity_id
           JOIN entities e ON e.id = seed.entity_id
           JOIN memories m ON m.id = nbr.memory_id
          WHERE seed.memory_id IN ({ph})
            AND nbr.memory_id NOT IN ({ph})
            AND m.superseded_by IS NULL
            AND m.deleted_at IS NULL
          ORDER BY m.created_at DESC, m.id, e.name",
        ph = ph
    );

    let mut bindings = text_params(seed_ids);
    bindings.extend(text_params(seed_ids));
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, String, String, String, String)> = stmt
        .query_map(params_from_iter(bindings), |row| {
            Ok((
                row.get("id")?,
                row.get("content")?,
                row.get("category")?,
                row.get("created_at")?,
                row.get("entity_name")?,
            ))
        })?
        .collect::<Result<_>>()?;

    // One row per (memory, entity) pair, so the entity names are gathered onto
    // a single item rather than producing the same memory five times.
    let mut order: Vec<String> = Vec::new();
    let mut items: std::collections::HashMap<String, RelatedMemory> =
        std::collections::HashMap::new();
    for (id, content, category, created_at, entity_name) in rows {
        if !items.contains_key(&id) {
            if order.len() >= EXPANSION_CAP {
                continue;
            }
            order.push(id.clone());
            items.insert(
                id.clone(),
                RelatedMemory::new(id.clone(), &content, category, created_at),
            );
        }
        let item = items.get_mut(&id).expect("just inserted");
        if !item.via_entities.contains(&entity_name) {
            item.via_entities.push(entity_name);
        }
    }

    Ok(order
        .into_iter()
        .filter_map(|id| items.remove(&id))
        .collect())
}

/// Sibling chunks of the same source document, within [`NEIGHBOR_WINDOW`]
/// positions of a seed.
///
/// Skips any seed without a `doc_id` — a manually added memory is not part of
/// a document, so it has no siblings. On a store with no importers this
/// returns nothing at all.
pub fn expand_via_neighbors(
    conn: &Connection,
    seeds: &[MemorySearchResult],
) -> Result<Vec<RelatedMemory>> {
    let seed_ids: std::collections::HashSet<&str> =
        seeds.iter().map(|s| s.memory.id.as_str()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();

    for seed in seeds {
        if items.len() >= EXPANSION_CAP {
            break;
        }
        let (doc_id, chunk_index) = match (&seed.memory.doc_id, seed.memory.chunk_index) {
            (Some(doc), Some(chunk)) => (doc, chunk),
            _ => continue,
        };

        let mut stmt = conn.prepare(
            "SELECT id, content, category, created_at, doc_id, chunk_index
               FROM memories
              WHERE doc_id = ?
                AND chunk_index BETWEEN ? AND ?
                AND superseded_by IS NULL
                AND deleted_at IS NULL
              ORDER BY chunk_index",
        )?;
        let rows: Vec<NeighborRow> = stmt
            .query_map(
                params![
                    doc_id,
                    chunk_index - NEIGHBOR_WINDOW,
                    chunk_index + NEIGHBOR_WINDOW
                ],
                |row| {
                    Ok(NeighborRow {
                        id: row.get("id")?,
                        content: row.get("content")?,
                        category: row.get("category")?,
                        created_at: row.get("created_at")?,
                        doc_id: row.get("doc_id")?,
                        chunk_index: row.get("chunk_index")?,
                    })
                },
            )?
            .collect::<Result<_>>()?;

        for row in rows {
            if seed_ids.contains(row.id.as_str()) || !seen.insert(row.id.clone()) {
                continue;
            }
            if items.len() >= EXPANSION_CAP {
                break;
            }
            let mut item =
                RelatedMemory::new(row.id.clone(), &row.content, row.category, row.created_at);
            item.doc_id = row.doc_id;
            item.chunk_index = row.chunk_index;
            items.push(item);
        }
    }

    Ok(items)
}

/// Memories most strongly co-retrieved with the seeds, strongest first.
///
/// Reads both sides of each association, since a pair is stored once under a
/// canonical order and either endpoint could be the seed.
pub fn expand_via_co_retrieval(
    conn: &Connection,
    seed_ids: &[String],
) -> Result<Vec<RelatedMemory>> {
    if seed_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ph = placeholders(seed_ids.len());
    let sql = format!(
        "SELECT assoc.other_id, assoc.weight, m.content, m.category, m.created_at
           FROM (
                SELECT memory_id_b AS other_id, weight FROM memory_associations
                 WHERE memory_id_a IN ({ph})
                UNION ALL
                SELECT memory_id_a AS other_id, weight FROM memory_associations
                 WHERE memory_id_b IN ({ph})
           ) assoc
           JOIN memories m ON m.id = assoc.other_id
          WHERE m.superseded_by IS NULL AND m.deleted_at IS NULL
          ORDER BY assoc.weight DESC, m.created_at DESC",
        ph = ph
    );

    let mut bindings = text_params(seed_ids);
    bindings.extend(text_params(seed_ids));
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, i64, String, String, String)> = stmt
        .query_map(params_from_iter(bindings), |row| {
            Ok((
                row.get("other_id")?,
                row.get("weight")?,
                row.get("content")?,
                row.get("category")?,
                row.get("created_at")?,
            ))
        })?
        .collect::<Result<_>>()?;

    let seeds: std::collections::HashSet<&str> = seed_ids.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    for (id, weight, content, category, created_at) in rows {
        if seeds.contains(id.as_str()) || !seen.insert(id.clone()) {
            continue;
        }
        if items.len() >= EXPANSION_CAP {
            break;
        }
        let mut item = RelatedMemory::new(id, &content, category, created_at);
        item.co_retrieval_weight = Some(weight);
        items.push(item);
    }

    Ok(items)
}
