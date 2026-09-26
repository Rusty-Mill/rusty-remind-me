//! Vault hygiene: finding clusters of near-duplicate memories and merging
//! them into one canonical representative.
//!
//! Mirrors the reference's split between `consolidation.py` (pure clustering
//! and merge functions, no DB access) and `tools/lifecycle.py` (the
//! `remind_me_consolidate` handler that fetches candidates, calls the pure
//! functions, and writes the merge back). [`find_clusters`], [`pick_canonical`]
//! and [`merge_cluster`] below are that pure layer — testable with plain
//! structs and no [`Connection`] — and [`consolidate`] is the thin
//! DB-touching orchestration around them.
//!
//! Key concepts:
//!   - **Clustering**: groups memories whose pairwise cosine similarity
//!     exceeds a threshold, using Union-Find for transitive closure — A~B and
//!     B~C clusters A, B and C together even if cos(A, C) itself falls short.
//!   - **Canonical selection**: the highest-vitality memory in each cluster
//!     becomes the canonical representative, tie-broken by most recent
//!     `accessed_at`.
//!   - **Merging**: combines content from cluster members into the canonical
//!     memory, deduplicating lines and summing access counts, then supersedes
//!     the rest via the same `superseded_by` mechanism
//!     [`crate::entity::supersede_contradicting_facts`] already uses.

use crate::db::memories::Memories;
use crate::db::vectors::Vectors;
use crate::models::ConsolidateInput;
use crate::vitality::calculate_vitality;
use chrono::Utc;

use rusqlite::{Connection, Result as SqlResult};
use serde_json::{json, Value as Json};
use std::collections::HashMap;

/// Hard cap on how many candidates [`find_clusters`] pairwise-compares in one
/// call. `ConsolidateInput::limit` (default 500, max
/// [`crate::CONSOLIDATE_LIMIT_MAX`]) already bounds the candidate pool
/// fetched from the DB; this is a second, independent ceiling on the
/// clustering step itself, matching the reference's
/// `CONSOLIDATE_MAX_CANDIDATES` — the O(n^2) comparison cost is what it
/// exists to bound, not merely the SQL `LIMIT`.
pub const CONSOLIDATE_MAX_CANDIDATES: usize = 1500;

/// Characters of content shown in a dry-run cluster report, matching the
/// reference's snippet length.
const REPORT_SNIPPET_CHARS: usize = 200;

/// One memory's worth of state the clustering/merge algorithms need — a
/// deliberately bare-bones stand-in for [`crate::models::Memory`] so this
/// module stays pure and DB-free.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterMember {
    pub id: String,
    pub content: String,
    pub vitality: f64,
    pub access_count: i64,
    pub accessed_at: String,
    pub tags: Vec<String>,
    pub decay_rate: f64,
    pub base_weight: f64,
}

/// Disjoint-set with path compression and union by rank, used to transitively
/// cluster indices whose pairwise similarity clears the threshold.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, x: usize, y: usize) {
        let (mut rx, mut ry) = (self.find(x), self.find(y));
        if rx == ry {
            return;
        }
        if self.rank[rx] < self.rank[ry] {
            std::mem::swap(&mut rx, &mut ry);
        }
        self.parent[ry] = rx;
        if self.rank[rx] == self.rank[ry] {
            self.rank[rx] += 1;
        }
    }
}

/// Cosine similarity between two equal-length vectors. Stored embeddings are
/// already L2-normalized at embed time (see [`crate::vectors`]), so this is a
/// plain dot product — the same convention `semantic_search` relies on.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum()
}

/// Find clusters of similar memories via transitive Union-Find over pairwise
/// cosine similarity.
///
/// Memories beyond `max_candidates` are dropped rather than compared — the
/// O(n^2) cost of the pairwise scan grows unboundedly otherwise. Only
/// clusters with 2+ members are returned, each sorted by vitality descending
/// (stable, so members tied on vitality keep their input order). Cluster
/// order itself is first-encountered-root order, matching the reference's
/// dict-based grouping.
///
/// A memory with no entry in `embeddings` cannot be compared to anything and
/// never joins a cluster — it is silently excluded rather than erroring,
/// since an un-embedded row is simply not a consolidation candidate yet.
pub fn find_clusters(
    memories: &[ClusterMember],
    embeddings: &HashMap<String, Vec<f32>>,
    similarity_threshold: f64,
    max_candidates: usize,
) -> Vec<Vec<ClusterMember>> {
    let memories = if memories.len() > max_candidates {
        &memories[..max_candidates]
    } else {
        memories
    };

    let n = memories.len();
    if n < 2 {
        return Vec::new();
    }

    let mut uf = UnionFind::new(n);
    for (i, member_i) in memories.iter().enumerate() {
        let Some(vi) = embeddings.get(&member_i.id) else {
            continue;
        };
        for (j, member_j) in memories.iter().enumerate().skip(i + 1) {
            let Some(vj) = embeddings.get(&member_j.id) else {
                continue;
            };
            if vi.len() != vj.len() {
                continue;
            }
            if cosine_similarity(vi, vj) >= similarity_threshold {
                uf.union(i, j);
            }
        }
    }

    // Group by root, preserving first-encountered order (matches the
    // reference's plain-dict grouping) rather than a HashMap's unspecified
    // iteration order.
    let mut root_order: Vec<usize> = Vec::new();
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        if !groups.contains_key(&root) {
            root_order.push(root);
        }
        groups.entry(root).or_default().push(i);
    }

    let mut clusters = Vec::new();
    for root in root_order {
        let indices = &groups[&root];
        if indices.len() < 2 {
            continue;
        }
        let mut cluster: Vec<ClusterMember> =
            indices.iter().map(|&i| memories[i].clone()).collect();
        cluster.sort_by(|a, b| {
            b.vitality
                .partial_cmp(&a.vitality)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        clusters.push(cluster);
    }
    clusters
}

/// Select the canonical (representative) memory from a cluster: highest
/// vitality, tie-broken by the most recent `accessed_at`.
///
/// Returns the *first* maximal element on a tie, matching Python's `max()` —
/// `Iterator::max_by` would instead keep the *last* one, which would silently
/// diverge from the reference on an exact tie.
///
/// `None` for an empty cluster; every real caller only ever passes a cluster
/// [`find_clusters`] already filtered to 2+ members, so this is unreachable
/// in practice, and returning `Option` avoids a panic on the reference's
/// `ValueError` in the one case that isn't.
pub fn pick_canonical(cluster: &[ClusterMember]) -> Option<&ClusterMember> {
    let mut best: Option<&ClusterMember> = None;
    for member in cluster {
        best = match best {
            None => Some(member),
            Some(current) => {
                let better = (member.vitality, member.accessed_at.as_str())
                    > (current.vitality, current.accessed_at.as_str());
                if better {
                    Some(member)
                } else {
                    Some(current)
                }
            }
        };
    }
    best
}

/// Result of merging cluster members into a canonical memory.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterMerge {
    pub merged_content: String,
    pub total_access_count: i64,
    pub superseded_ids: Vec<String>,
    pub merged_tags: Vec<String>,
}

/// Merge cluster members into the canonical memory.
///
/// Without a `summary`, merged clusters would grow unbounded — content is
/// simply the deduplicated union of every member's lines, canonical first.
/// When `summary` is given (an LLM-authored distillation of the cluster,
/// produced client-side exactly like `remind_me_decompose`/
/// `remind_me_normalize_apply` already are) it replaces the raw union
/// entirely as `merged_content`. [`consolidate`]'s auto-merge path always
/// supplies one; the no-summary fallback exists for direct callers of this
/// pure function — tests, or any caller with no LLM in the loop that just
/// wants the union.
pub fn merge_cluster(
    canonical: &ClusterMember,
    members: &[ClusterMember],
    summary: Option<&str>,
) -> ClusterMerge {
    let merged_content = match summary {
        Some(s) => s.to_string(),
        None => {
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut lines: Vec<&str> = Vec::new();
            for line in canonical
                .content
                .split('\n')
                .chain(members.iter().flat_map(|m| m.content.split('\n')))
            {
                if seen.insert(line) {
                    lines.push(line);
                }
            }
            lines.join("\n")
        }
    };

    let total_access_count =
        canonical.access_count + members.iter().map(|m| m.access_count).sum::<i64>();
    let superseded_ids = members.iter().map(|m| m.id.clone()).collect();

    let mut seen_tags: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut merged_tags: Vec<String> = Vec::new();
    for tag in canonical
        .tags
        .iter()
        .chain(members.iter().flat_map(|m| m.tags.iter()))
    {
        if seen_tags.insert(tag.as_str()) {
            merged_tags.push(tag.clone());
        }
    }

    ClusterMerge {
        merged_content,
        total_access_count,
        superseded_ids,
        merged_tags,
    }
}

// ---------------------------------------------------------------------------
// DB-touching orchestration
// ---------------------------------------------------------------------------

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Round to 4 decimal places, matching the reference's dry-run similarity
/// display.
fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// A candidate pool: the members themselves, plus each one's chunk-0
/// embedding keyed by memory id.
type Candidates = (Vec<ClusterMember>, HashMap<String, Vec<f32>>);

/// Active, non-superseded, non-deleted memories with a chunk-0 embedding —
/// the same candidate set the reference's SQL join selects — optionally
/// scoped to `category`, capped at `limit`.
fn fetch_candidates(
    conn: &Connection,
    category: Option<&str>,
    limit: usize,
) -> SqlResult<Candidates> {
    let rows = Vectors::new(conn).consolidation_candidates(category, limit)?;

    let mut members = Vec::with_capacity(rows.len());
    let mut embeddings = HashMap::new();
    for row in rows {
        embeddings.insert(
            row.id.clone(),
            crate::vectors::le_bytes_to_f32(&row.embedding),
        );
        members.push(ClusterMember {
            id: row.id,
            content: row.content,
            vitality: row.vitality,
            access_count: row.access_count,
            accessed_at: row.accessed_at,
            tags: row.tags,
            decay_rate: row.decay_rate,
            base_weight: row.base_weight,
        });
    }
    Ok((members, embeddings))
}

/// Find clusters of semantically similar memories and, unless `dry_run`,
/// merge them.
///
/// In dry-run mode (the default), reports each cluster's canonical and
/// member details — including each member's similarity to the canonical —
/// without touching the store. In auto-merge mode, a cluster only merges
/// when its canonical id has a matching entry in `input.summaries`; a
/// cluster found but missing a summary is skipped and listed separately,
/// never silently merged with a raw line union (the reference's issue #55
/// fix). A merge updates the canonical's content/access_count/tags/vitality,
/// supersedes every other member via `superseded_by`, and best-effort
/// re-embeds the canonical with its merged content.
///
/// Only active, non-superseded memories are considered.
pub fn consolidate(conn: &Connection, input: &ConsolidateInput) -> SqlResult<Json> {
    let similarity_threshold = input.similarity_threshold.clamp(
        crate::CONSOLIDATE_SIMILARITY_MIN,
        crate::CONSOLIDATE_SIMILARITY_MAX,
    );
    let limit = input
        .limit
        .clamp(crate::CONSOLIDATE_LIMIT_MIN, crate::CONSOLIDATE_LIMIT_MAX);

    let (members, embeddings) = fetch_candidates(conn, input.category.as_deref(), limit)?;
    if members.is_empty() {
        return Ok(json!({ "clusters_found": 0, "message": "No eligible memories found" }));
    }

    let clusters = find_clusters(
        &members,
        &embeddings,
        similarity_threshold,
        CONSOLIDATE_MAX_CANDIDATES,
    );
    if clusters.is_empty() {
        return Ok(json!({
            "clusters_found": 0,
            "message": "No similar memories found above threshold"
        }));
    }

    if input.dry_run {
        return Ok(dry_run_report(&clusters, &embeddings));
    }

    apply_merges(conn, &clusters, input.summaries.as_ref())
}

fn dry_run_report(clusters: &[Vec<ClusterMember>], embeddings: &HashMap<String, Vec<f32>>) -> Json {
    let mut cluster_reports = Vec::with_capacity(clusters.len());
    for cluster in clusters {
        // `find_clusters` only ever returns clusters with 2+ members, so this
        // is always `Some` here.
        let Some(canonical) = pick_canonical(cluster) else {
            continue;
        };
        let canonical_vec = embeddings.get(&canonical.id);

        let member_reports: Vec<Json> = cluster
            .iter()
            .filter(|m| m.id != canonical.id)
            .map(|member| {
                let similarity = match (canonical_vec, embeddings.get(&member.id)) {
                    (Some(a), Some(b)) if a.len() == b.len() => round4(cosine_similarity(a, b)),
                    _ => 0.0,
                };
                json!({
                    "id": member.id,
                    "content": truncate_chars(&member.content, REPORT_SNIPPET_CHARS),
                    "vitality": member.vitality,
                    "similarity": similarity,
                })
            })
            .collect();

        cluster_reports.push(json!({
            "canonical": {
                "id": canonical.id,
                "content": truncate_chars(&canonical.content, REPORT_SNIPPET_CHARS),
                "vitality": canonical.vitality,
            },
            "members": member_reports,
            "cluster_size": cluster.len(),
        }));
    }

    json!({
        "clusters_found": clusters.len(),
        "dry_run": true,
        "clusters": cluster_reports,
    })
}

fn apply_merges(
    conn: &Connection,
    clusters: &[Vec<ClusterMember>],
    summaries: Option<&HashMap<String, String>>,
) -> SqlResult<Json> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let empty_summaries = HashMap::new();
    let summaries = summaries.unwrap_or(&empty_summaries);

    let mut total_superseded = 0usize;
    let mut canonical_ids: Vec<String> = Vec::new();
    let mut skipped_no_summary: Vec<String> = Vec::new();
    let embedder = crate::embedder::available_embedder();

    for cluster in clusters {
        let Some(canonical) = pick_canonical(cluster) else {
            continue;
        };
        let Some(summary) = summaries.get(&canonical.id) else {
            // No LLM-authored summary for this cluster (issue #55): skip it
            // rather than falling back to a raw line-union merge.
            skipped_no_summary.push(canonical.id.clone());
            continue;
        };

        let member_refs: Vec<ClusterMember> = cluster
            .iter()
            .filter(|m| m.id != canonical.id)
            .cloned()
            .collect();
        let merged = merge_cluster(canonical, &member_refs, Some(summary.as_str()));

        let memories = Memories::new(conn);
        memories.set_merged(
            &canonical.id,
            &merged.merged_content,
            merged.total_access_count,
            &merged.merged_tags,
            &now_iso,
        )?;

        // Recompute vitality for the canonical at zero elapsed days, the same
        // write-time snapshot `add_memory` seeds — `calculate_vitality`
        // already applies bridge protection internally from `access_count`.
        let new_vitality = calculate_vitality(
            canonical.base_weight,
            merged.total_access_count,
            canonical.decay_rate,
            &now_iso,
            now,
        );
        memories.set_vitality(&canonical.id, new_vitality, "active")?;

        for member_id in &merged.superseded_ids {
            memories.set_superseded_by(member_id, &canonical.id, Some(&now_iso))?;
        }

        total_superseded += merged.superseded_ids.len();
        canonical_ids.push(canonical.id.clone());

        // Best-effort re-embed with the merged content, same as every other
        // content-mutating write in this crate (`add_memory`,
        // `apply_normalizations`) — no embedder configured, or one that
        // fails mid-request, is never a reason to fail a merge that already
        // succeeded. Synchronous rather than the reference's fire-and-forget
        // background task: this crate has no async runtime to spawn one on.
        if let Some(embedder) = embedder.as_ref() {
            let _ = crate::vectors::embed_and_store(
                conn,
                &**embedder,
                &canonical.id,
                &merged.merged_content,
            );
        }
    }

    Ok(json!({
        "clusters_found": clusters.len(),
        "clusters_merged": canonical_ids.len(),
        "memories_superseded": total_superseded,
        "canonical_ids": canonical_ids,
        "skipped_no_summary": skipped_no_summary,
        "dry_run": false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, vitality: f64, accessed_at: &str, access_count: i64) -> ClusterMember {
        ClusterMember {
            id: id.to_string(),
            content: format!("content for {id}"),
            vitality,
            access_count,
            accessed_at: accessed_at.to_string(),
            tags: vec![],
            decay_rate: 0.1,
            base_weight: 1.0,
        }
    }

    fn embedding(vec: Vec<f32>) -> Vec<f32> {
        vec
    }

    #[test]
    fn pairs_below_the_threshold_do_not_cluster() {
        let memories = vec![
            member("a", 1.0, "2026-01-01T00:00:00Z", 0),
            member("b", 1.0, "2026-01-01T00:00:00Z", 0),
        ];
        let mut embeddings = HashMap::new();
        embeddings.insert("a".to_string(), embedding(vec![1.0, 0.0]));
        embeddings.insert("b".to_string(), embedding(vec![0.0, 1.0]));

        let clusters = find_clusters(&memories, &embeddings, 0.85, CONSOLIDATE_MAX_CANDIDATES);

        assert!(clusters.is_empty());
    }

    #[test]
    fn a_pair_above_the_threshold_clusters() {
        let memories = vec![
            member("a", 1.0, "2026-01-01T00:00:00Z", 0),
            member("b", 0.5, "2026-01-01T00:00:00Z", 0),
        ];
        let mut embeddings = HashMap::new();
        embeddings.insert("a".to_string(), embedding(vec![1.0, 0.0]));
        embeddings.insert("b".to_string(), embedding(vec![0.99, 0.14107]));

        let clusters = find_clusters(&memories, &embeddings, 0.85, CONSOLIDATE_MAX_CANDIDATES);

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), 2);
    }

    #[test]
    fn transitive_similarity_clusters_a_chain_even_when_the_ends_fall_short() {
        // cos(a, b) and cos(b, c) both clear the threshold, but cos(a, c) does
        // not -- Union-Find must still put all three in one cluster.
        let memories = vec![
            member("a", 1.0, "2026-01-01T00:00:00Z", 0),
            member("b", 1.0, "2026-01-01T00:00:00Z", 0),
            member("c", 1.0, "2026-01-01T00:00:00Z", 0),
        ];
        // a=0deg, b=20deg, c=40deg: cos(20deg)~=0.94, cos(40deg)~=0.766 < threshold.
        let mut embeddings = HashMap::new();
        embeddings.insert("a".to_string(), embedding(vec![1.0, 0.0]));
        embeddings.insert(
            "b".to_string(),
            embedding(vec![20f32.to_radians().cos(), 20f32.to_radians().sin()]),
        );
        embeddings.insert(
            "c".to_string(),
            embedding(vec![40f32.to_radians().cos(), 40f32.to_radians().sin()]),
        );

        let direct_ac = cosine_similarity(&embeddings["a"], &embeddings["c"]);
        assert!(
            direct_ac < 0.9,
            "a and c must not directly clear the threshold"
        );

        let clusters = find_clusters(&memories, &embeddings, 0.9, CONSOLIDATE_MAX_CANDIDATES);

        assert_eq!(clusters.len(), 1);
        let ids: std::collections::HashSet<&str> =
            clusters[0].iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"].into_iter().collect());
    }

    #[test]
    fn candidates_beyond_max_are_dropped_rather_than_compared() {
        let memories = vec![
            member("a", 1.0, "2026-01-01T00:00:00Z", 0),
            member("b", 1.0, "2026-01-01T00:00:00Z", 0),
        ];
        let mut embeddings = HashMap::new();
        embeddings.insert("a".to_string(), embedding(vec![1.0, 0.0]));
        embeddings.insert("b".to_string(), embedding(vec![1.0, 0.0]));

        let clusters = find_clusters(&memories, &embeddings, 0.85, 1);

        assert!(
            clusters.is_empty(),
            "with max_candidates=1, only one memory survives -- nothing to pair"
        );
    }

    #[test]
    fn canonical_selection_prefers_highest_vitality() {
        let cluster = vec![
            member("low", 0.2, "2026-01-01T00:00:00Z", 0),
            member("high", 0.9, "2026-01-01T00:00:00Z", 0),
            member("mid", 0.5, "2026-01-01T00:00:00Z", 0),
        ];

        let canonical = pick_canonical(&cluster).unwrap();

        assert_eq!(canonical.id, "high");
    }

    #[test]
    fn canonical_selection_ties_break_on_most_recent_accessed_at() {
        let cluster = vec![
            member("older", 0.9, "2026-01-01T00:00:00Z", 0),
            member("newer", 0.9, "2026-06-01T00:00:00Z", 0),
        ];

        let canonical = pick_canonical(&cluster).unwrap();

        assert_eq!(canonical.id, "newer");
    }

    #[test]
    fn canonical_selection_keeps_the_first_of_an_exact_tie() {
        // Matches Python's max(): the first maximal element wins, not the
        // last -- Rust's Iterator::max_by would pick the opposite one.
        let cluster = vec![
            member("first", 0.9, "2026-01-01T00:00:00Z", 0),
            member("second", 0.9, "2026-01-01T00:00:00Z", 0),
        ];

        let canonical = pick_canonical(&cluster).unwrap();

        assert_eq!(canonical.id, "first");
    }

    #[test]
    fn pick_canonical_on_an_empty_cluster_is_none() {
        assert!(pick_canonical(&[]).is_none());
    }

    #[test]
    fn merge_without_a_summary_deduplicates_lines_canonical_first() {
        let canonical = ClusterMember {
            content: "line one\nline two".to_string(),
            ..member("canonical", 0.9, "2026-01-01T00:00:00Z", 3)
        };
        let dup = ClusterMember {
            content: "line two\nline three".to_string(),
            ..member("dup", 0.5, "2026-01-01T00:00:00Z", 2)
        };

        let merged = merge_cluster(&canonical, &[dup], None);

        assert_eq!(merged.merged_content, "line one\nline two\nline three");
    }

    #[test]
    fn merge_with_a_summary_replaces_the_content_entirely() {
        let canonical = member("canonical", 0.9, "2026-01-01T00:00:00Z", 1);
        let dup = member("dup", 0.5, "2026-01-01T00:00:00Z", 1);

        let merged = merge_cluster(&canonical, &[dup], Some("a distilled summary"));

        assert_eq!(merged.merged_content, "a distilled summary");
    }

    #[test]
    fn merge_sums_access_counts_across_every_member() {
        let canonical = member("canonical", 0.9, "2026-01-01T00:00:00Z", 5);
        let dup_a = member("a", 0.5, "2026-01-01T00:00:00Z", 3);
        let dup_b = member("b", 0.4, "2026-01-01T00:00:00Z", 2);

        let merged = merge_cluster(&canonical, &[dup_a, dup_b], Some("summary"));

        assert_eq!(merged.total_access_count, 10);
    }

    #[test]
    fn merge_reports_every_member_as_superseded() {
        let canonical = member("canonical", 0.9, "2026-01-01T00:00:00Z", 1);
        let dup_a = member("a", 0.5, "2026-01-01T00:00:00Z", 1);
        let dup_b = member("b", 0.4, "2026-01-01T00:00:00Z", 1);

        let merged = merge_cluster(&canonical, &[dup_a, dup_b], Some("summary"));

        assert_eq!(
            merged.superseded_ids,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn merge_tags_are_order_preserving_and_deduplicated() {
        let canonical = ClusterMember {
            tags: vec!["a".to_string(), "b".to_string()],
            ..member("canonical", 0.9, "2026-01-01T00:00:00Z", 1)
        };
        let dup = ClusterMember {
            tags: vec!["b".to_string(), "c".to_string()],
            ..member("dup", 0.5, "2026-01-01T00:00:00Z", 1)
        };

        let merged = merge_cluster(&canonical, &[dup], Some("summary"));

        assert_eq!(
            merged.merged_tags,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }
}
