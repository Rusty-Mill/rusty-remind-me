//! Hierarchical embedding routing, the spike for Mill Board #1563
//! (`docs/research/1563-routing-spike.md`).
//!
//! Routed search descends a tree of cluster centroids: the query is scored
//! against the top-level centroids, the best `beam` nodes are expanded into
//! their children, and so on until only leaves are left; then the members of
//! those leaves are ranked exactly. Flat search scores every memory. Both
//! use the same per-memory unit vectors ([`topic_seed::memory_vectors`]), so
//! any difference between them is the routing's alone.
//!
//! Offline only. Nothing here is on the live search path, and nothing writes
//! the store: [`eval_on`] reads it under one short handle, drops it, then
//! embeds the queries and measures in memory.

use crate::db::memories::Memories;
use crate::db::{Database, Store};
use crate::embedder::{EmbedRole, Embedder};
use crate::retrieval::estimated_content_tokens;
use crate::topic_seed::{self, dot, normalize, ClusterNode, SeedEmbedding, SeedParams, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

/// The deepest rank measured; recall and tokens are reported for k up to it.
pub const MAX_K: usize = 50;
/// Go rule (Nano, 2026-11-18, D23): routed recall@10 may sit at most this far
/// below flat recall@10.
pub const RECALL_SLACK: f64 = 0.02;
/// Go rule: routed search must reach that recall on at most this share of
/// flat@10's tokens (at least 30% fewer).
pub const TOKEN_SHARE: f64 = 0.70;
/// Go rule: routed p95 latency may be at most this many times flat p95.
pub const LATENCY_RATIO: f64 = 2.0;
/// Beam widths measured when none are given.
pub const DEFAULT_BEAMS: [usize; 4] = [1, 2, 4, 8];
/// Queries per embedding call.
const EMBED_BATCH: usize = 16;

// --- corpus ----------------------------------------------------------------

/// The memories search runs over: an id, a unit vector and a token cost each,
/// addressed by position.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Corpus {
    pub ids: Vec<String>,
    pub vectors: Vec<Vec<f32>>,
    pub tokens: Vec<usize>,
}

impl Corpus {
    /// Build from `(id, unit vector, tokens)` rows, in the order given.
    pub fn new(rows: Vec<(String, Vec<f32>, usize)>) -> Self {
        let mut corpus = Self::default();
        for (id, vector, tokens) in rows {
            corpus.ids.push(id);
            corpus.vectors.push(vector);
            corpus.tokens.push(tokens);
        }
        corpus
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The vectors' dimension, 0 for an empty corpus.
    pub fn dim(&self) -> usize {
        self.vectors.first().map_or(0, Vec::len)
    }

    fn positions(&self) -> HashMap<&str, usize> {
        self.ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect()
    }
}

// --- the tree --------------------------------------------------------------

/// One node of a [`RouteIndex`].
#[derive(Debug, Clone, PartialEq)]
pub struct RouteNode {
    pub parent: Option<usize>,
    pub label: Option<String>,
    /// Unit vector; empty when no member is in the corpus, and then the node
    /// is never chosen.
    pub centroid: Vec<f32>,
    /// Corpus positions, on a leaf only.
    pub members: Vec<usize>,
}

/// A tree of centroids over a [`Corpus`].
#[derive(Debug, Clone, PartialEq)]
pub struct RouteIndex {
    nodes: Vec<RouteNode>,
    children: Vec<Vec<usize>>,
    tops: Vec<usize>,
}

impl RouteIndex {
    fn from_nodes(nodes: Vec<RouteNode>) -> Self {
        let mut children = vec![Vec::new(); nodes.len()];
        let mut tops = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            match node.parent {
                Some(p) => children[p].push(i),
                None => tops.push(i),
            }
        }
        Self {
            nodes,
            children,
            tops,
        }
    }

    /// From [`topic_seed::cluster_tree`] run over the corpus's own vectors
    /// (so cluster member indices are corpus positions). The root is dropped:
    /// its children become the top level.
    pub fn from_clusters(tree: &[ClusterNode]) -> Self {
        let is_parent = |i: usize| tree.iter().any(|n| n.parent == Some(i));
        let nodes = tree
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, node)| RouteNode {
                parent: node.parent.filter(|&p| p != 0).map(|p| p - 1),
                label: None,
                centroid: normalize(node.centroid.clone()),
                members: if is_parent(i) {
                    Vec::new()
                } else {
                    node.members.clone()
                },
            })
            .collect();
        Self::from_nodes(nodes)
    }

    /// From a saved seed run. A node's centroid is the unit mean of the corpus
    /// vectors of every leaf member below it, so a run named and applied
    /// later routes the same way. Members no longer in the corpus are
    /// skipped; memories added since the run sit in no leaf
    /// ([`RouteIndex::routed`] counts the ones that do).
    pub fn from_snapshot(snapshot: &Snapshot, corpus: &Corpus) -> Self {
        let at: HashMap<usize, usize> = snapshot
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id, i))
            .collect();
        let positions = corpus.positions();
        let dim = corpus.dim();
        let mut sums = vec![vec![0.0f32; dim]; snapshot.nodes.len()];
        // Members in the corpus at or below each node: a nonempty subtree
        // whose vectors cancel still gets a (zero) centroid of the right
        // dimension, so it can be chosen; only an empty one gets none.
        let mut counts = vec![0usize; snapshot.nodes.len()];
        let mut nodes: Vec<RouteNode> = snapshot
            .nodes
            .iter()
            .map(|n| RouteNode {
                parent: n.parent.and_then(|p| at.get(&p).copied()),
                label: n.label.clone(),
                centroid: Vec::new(),
                members: n
                    .members
                    .iter()
                    .filter_map(|id| positions.get(id.as_str()).copied())
                    .collect(),
            })
            .collect();
        // Breadth first, so a child always comes after its parent: walking
        // backwards finishes each subtree's sum before adding it upward.
        for i in (0..nodes.len()).rev() {
            for &m in &nodes[i].members {
                add_into(&mut sums[i], &corpus.vectors[m]);
            }
            counts[i] += nodes[i].members.len();
            if let Some(p) = nodes[i].parent {
                let child = sums[i].clone();
                add_into(&mut sums[p], &child);
                counts[p] += counts[i];
            }
        }
        for ((node, sum), count) in nodes.iter_mut().zip(sums).zip(counts) {
            if count > 0 {
                node.centroid = normalize(sum);
            }
        }
        Self::from_nodes(nodes)
    }

    pub fn nodes(&self) -> &[RouteNode] {
        &self.nodes
    }

    pub fn leaves(&self) -> usize {
        (0..self.nodes.len()).filter(|&i| self.is_leaf(i)).count()
    }

    /// Levels below the root, 0 for an empty tree.
    pub fn depth(&self) -> usize {
        (0..self.nodes.len())
            .map(|mut i| {
                let mut d = 1;
                while let Some(p) = self.nodes[i].parent {
                    d += 1;
                    i = p;
                }
                d
            })
            .max()
            .unwrap_or(0)
    }

    /// Corpus memories that sit in some leaf, so routing can reach them.
    pub fn routed(&self) -> usize {
        self.nodes.iter().map(|n| n.members.len()).sum()
    }

    fn is_leaf(&self, node: usize) -> bool {
        self.children[node].is_empty()
    }

    fn score(&self, nodes: &[usize], query: &[f32], scored: &mut usize) -> Vec<Step> {
        nodes
            .iter()
            .map(|&node| {
                let centroid = &self.nodes[node].centroid;
                let score = if centroid.len() == query.len() {
                    *scored += 1;
                    dot(query, centroid)
                } else {
                    f32::NEG_INFINITY
                };
                Step { node, score }
            })
            .collect()
    }

    /// Beam search down the tree: keep the best `beam` nodes (at least 1),
    /// replace each interior one by its children, score, keep the best
    /// `beam` again, until every kept node is a leaf. Pure.
    pub fn descend(&self, query: &[f32], beam: usize) -> Descent {
        let beam = beam.max(1);
        let mut centroids_scored = 0;
        let mut frontier = self.score(&self.tops, query, &mut centroids_scored);
        keep_best(&mut frontier, beam);
        let mut levels = vec![frontier.clone()];
        while frontier.iter().any(|s| !self.is_leaf(s.node)) {
            let mut next = Vec::new();
            for step in &frontier {
                if self.is_leaf(step.node) {
                    next.push(*step);
                } else {
                    next.extend(self.score(
                        &self.children[step.node],
                        query,
                        &mut centroids_scored,
                    ));
                }
            }
            keep_best(&mut next, beam);
            levels.push(next.clone());
            frontier = next;
        }
        Descent {
            levels,
            leaves: frontier
                .iter()
                .filter(|s| s.score.is_finite())
                .map(|s| s.node)
                .collect(),
            centroids_scored,
        }
    }
}

fn add_into(sum: &mut [f32], v: &[f32]) {
    if sum.len() == v.len() {
        sum.iter_mut().zip(v).for_each(|(s, x)| *s += x);
    }
}

/// One node a descent kept, with the query's cosine to its centroid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub node: usize,
    pub score: f32,
}

/// The path one query took down a [`RouteIndex`].
#[derive(Debug, Clone, PartialEq)]
pub struct Descent {
    /// The nodes kept after each step, best first.
    pub levels: Vec<Vec<Step>>,
    /// The leaves whose members are ranked.
    pub leaves: Vec<usize>,
    pub centroids_scored: usize,
}

fn keep_best(steps: &mut Vec<Step>, n: usize) {
    steps.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    steps.truncate(n);
}

// --- ranking ---------------------------------------------------------------

/// The best corpus positions for a query, best first, and how many vectors
/// were scored to find them.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranking {
    pub hits: Vec<usize>,
    pub scored: usize,
}

fn top_k(
    candidates: impl Iterator<Item = usize>,
    corpus: &Corpus,
    query: &[f32],
    k: usize,
) -> Ranking {
    let mut scored: Vec<(usize, f32)> = candidates
        .filter(|&i| corpus.vectors[i].len() == query.len())
        .map(|i| (i, dot(query, &corpus.vectors[i])))
        .collect();
    let count = scored.len();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(k);
    Ranking {
        hits: scored.into_iter().map(|(i, _)| i).collect(),
        scored: count,
    }
}

/// Flat search: every memory scored exactly. Pure.
pub fn rank_flat(corpus: &Corpus, query: &[f32], k: usize) -> Ranking {
    top_k(0..corpus.len(), corpus, query, k)
}

/// Routed search: descend, then rank the chosen leaves' members exactly.
/// `scored` counts centroids and members. Pure.
pub fn rank_routed(
    index: &RouteIndex,
    corpus: &Corpus,
    query: &[f32],
    beam: usize,
    k: usize,
) -> (Ranking, Descent) {
    let descent = index.descend(query, beam);
    let members = descent
        .leaves
        .iter()
        .flat_map(|&leaf| index.nodes[leaf].members.iter().copied());
    let mut ranking = top_k(members, corpus, query, k);
    ranking.scored += descent.centroids_scored;
    (ranking, descent)
}

// --- metrics ---------------------------------------------------------------

/// Share of `relevant` found in the first `k` hits; 0 when nothing is relevant.
pub fn recall_at(hits: &[usize], relevant: &[usize], k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let top = &hits[..k.min(hits.len())];
    let found = relevant.iter().filter(|r| top.contains(r)).count();
    found as f64 / relevant.len() as f64
}

/// Tokens of the first `k` hits' content, the text handed to a model.
pub fn tokens_at(hits: &[usize], corpus: &Corpus, k: usize) -> usize {
    hits.iter().take(k).map(|&i| corpus.tokens[i]).sum()
}

/// Nearest-rank percentile `p` (0 to 100) of `values`; 0 when empty.
pub fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

// --- evaluation ------------------------------------------------------------

/// One labelled query, the line format of the #59 eval set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalQuery {
    pub query: String,
    pub relevant: Vec<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

/// One search path's averages over the eval set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathScores {
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub tokens_at_5: f64,
    pub tokens_at_10: f64,
    pub vectors_scored: f64,
    pub latency_p50_us: f64,
    pub latency_p95_us: f64,
    /// Mean recall at k = 1..=[`MAX_K`].
    #[serde(skip)]
    pub recall_curve: Vec<f64>,
    /// Mean tokens at k = 1..=[`MAX_K`].
    #[serde(skip)]
    pub tokens_curve: Vec<f64>,
}

/// The smallest k at which routed recall reaches flat recall@10 less
/// [`RECALL_SLACK`], and the tokens it hands over there.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Match {
    pub k: usize,
    pub tokens: f64,
}

/// The D23 go rule for one beam width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub recall_ok: bool,
    pub tokens_ok: bool,
    pub latency_ok: bool,
    pub go: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BeamScores {
    pub beam: usize,
    pub scores: PathScores,
    pub tokens_to_match: Option<Match>,
    pub verdict: Verdict,
}

/// What a run measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalReport {
    pub run: Option<String>,
    pub corpus: usize,
    pub nodes: usize,
    pub leaves: usize,
    pub depth: usize,
    /// Corpus memories in no leaf, out of routing's reach.
    pub unrouted: usize,
    pub queries: usize,
    /// Queries none of whose relevant memories is in the corpus.
    pub skipped: Vec<String>,
    /// Relevant ids, over all queries, that are not in the corpus.
    pub missing_relevant: usize,
    pub flat: PathScores,
    pub routed: Vec<BeamScores>,
}

/// One routing descent, kept as training data for a learned router.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DescentRecord {
    pub run: Option<String>,
    pub embedding: Option<SeedEmbedding>,
    pub query: String,
    pub beam: usize,
    /// The nodes kept after each step: `(node, label, score)`, best first.
    pub path: Vec<Vec<(usize, Option<String>, f32)>>,
    pub leaves: Vec<usize>,
    pub scored: usize,
    /// The first ten hits' memory ids, and whether each is relevant.
    pub top: Vec<String>,
    pub hits: Vec<bool>,
}

/// Where a run's tree and vectors came from, stamped on each descent record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Provenance {
    pub run: Option<String>,
    pub embedding: Option<SeedEmbedding>,
}

struct Row {
    hits: Vec<usize>,
    relevant: Vec<usize>,
    scored: usize,
    micros: f64,
}

fn scores(rows: &[Row], corpus: &Corpus) -> PathScores {
    let n = rows.len().max(1) as f64;
    let mean = |f: &dyn Fn(&Row) -> f64| rows.iter().map(f).sum::<f64>() / n;
    let recall_curve: Vec<f64> = (1..=MAX_K)
        .map(|k| mean(&|r| recall_at(&r.hits, &r.relevant, k)))
        .collect();
    let tokens_curve: Vec<f64> = (1..=MAX_K)
        .map(|k| mean(&|r| tokens_at(&r.hits, corpus, k) as f64))
        .collect();
    let micros: Vec<f64> = rows.iter().map(|r| r.micros).collect();
    PathScores {
        recall_at_5: recall_curve[4],
        recall_at_10: recall_curve[9],
        tokens_at_5: tokens_curve[4],
        tokens_at_10: tokens_curve[9],
        vectors_scored: mean(&|r| r.scored as f64),
        latency_p50_us: percentile(&micros, 50.0),
        latency_p95_us: percentile(&micros, 95.0),
        recall_curve,
        tokens_curve,
    }
}

/// The smallest k where `routed` recall reaches `flat` recall@10 less
/// [`RECALL_SLACK`]. Pure.
pub fn tokens_to_match(flat: &PathScores, routed: &PathScores) -> Option<Match> {
    let target = flat.recall_at_10 - RECALL_SLACK - 1e-9;
    routed
        .recall_curve
        .iter()
        .position(|&r| r >= target)
        .map(|i| Match {
            k: i + 1,
            tokens: routed.tokens_curve[i],
        })
}

/// Apply the D23 go rule. Pure.
pub fn verdict(flat: &PathScores, routed: &PathScores, matched: Option<Match>) -> Verdict {
    let recall_ok = routed.recall_at_10 >= flat.recall_at_10 - RECALL_SLACK - 1e-9;
    let tokens_ok = matched.is_some_and(|m| m.tokens <= TOKEN_SHARE * flat.tokens_at_10);
    let latency_ok = routed.latency_p95_us <= LATENCY_RATIO * flat.latency_p95_us;
    Verdict {
        recall_ok,
        tokens_ok,
        latency_ok,
        go: recall_ok && tokens_ok && latency_ok,
    }
}

fn record(
    query: &EvalQuery,
    beam: usize,
    index: &RouteIndex,
    corpus: &Corpus,
    row: &Row,
    descent: &Descent,
    from: &Provenance,
) -> DescentRecord {
    let top: Vec<usize> = row.hits.iter().take(10).copied().collect();
    DescentRecord {
        run: from.run.clone(),
        embedding: from.embedding.clone(),
        query: query.query.clone(),
        beam,
        path: descent
            .levels
            .iter()
            .map(|level| {
                level
                    .iter()
                    .map(|s| (s.node, index.nodes[s.node].label.clone(), s.score))
                    .collect()
            })
            .collect(),
        leaves: descent.leaves.clone(),
        scored: row.scored,
        hits: top.iter().map(|i| row.relevant.contains(i)).collect(),
        top: top.into_iter().map(|i| corpus.ids[i].clone()).collect(),
    }
}

/// Measure flat search and routed search at each beam width over `queries`
/// (each with its embedded vector), and the descent of every routed query.
/// Deterministic apart from the latencies.
pub fn evaluate(
    index: &RouteIndex,
    corpus: &Corpus,
    queries: &[(EvalQuery, Vec<f32>)],
    beams: &[usize],
    from: &Provenance,
) -> (EvalReport, Vec<DescentRecord>) {
    let positions = corpus.positions();
    let mut skipped = Vec::new();
    let mut missing_relevant = 0;
    let mut usable: Vec<(&EvalQuery, &[f32], Vec<usize>)> = Vec::new();
    for (query, vector) in queries {
        let relevant: Vec<usize> = query
            .relevant
            .iter()
            .filter_map(|id| positions.get(id.as_str()).copied())
            .collect();
        missing_relevant += query.relevant.len() - relevant.len();
        if relevant.is_empty() {
            skipped.push(query.query.clone());
        } else {
            usable.push((query, vector, relevant));
        }
    }

    let flat_rows: Vec<Row> = usable
        .iter()
        .map(|(_, vector, relevant)| {
            let started = Instant::now();
            let ranking = rank_flat(corpus, vector, MAX_K);
            Row {
                micros: started.elapsed().as_secs_f64() * 1e6,
                hits: ranking.hits,
                relevant: relevant.clone(),
                scored: ranking.scored,
            }
        })
        .collect();
    let flat = scores(&flat_rows, corpus);

    let mut descents = Vec::new();
    let mut routed = Vec::new();
    for &beam in beams {
        let mut rows = Vec::with_capacity(usable.len());
        for (query, vector, relevant) in &usable {
            let started = Instant::now();
            let (ranking, descent) = rank_routed(index, corpus, vector, beam, MAX_K);
            let row = Row {
                micros: started.elapsed().as_secs_f64() * 1e6,
                hits: ranking.hits,
                relevant: relevant.clone(),
                scored: ranking.scored,
            };
            descents.push(record(query, beam, index, corpus, &row, &descent, from));
            rows.push(row);
        }
        let path = scores(&rows, corpus);
        let matched = tokens_to_match(&flat, &path);
        // With no labelled query measured, every average is 0 and the rule
        // would pass against nothing: such a run is never a go.
        let verdict = match usable.is_empty() {
            true => Verdict {
                recall_ok: false,
                tokens_ok: false,
                latency_ok: false,
                go: false,
            },
            false => verdict(&flat, &path, matched),
        };
        routed.push(BeamScores {
            beam,
            verdict,
            tokens_to_match: matched,
            scores: path,
        });
    }

    let report = EvalReport {
        run: from.run.clone(),
        corpus: corpus.len(),
        nodes: index.nodes.len(),
        leaves: index.leaves(),
        depth: index.depth(),
        unrouted: corpus.len().saturating_sub(index.routed()),
        queries: usable.len(),
        skipped,
        missing_relevant,
        flat,
        routed,
    };
    (report, descents)
}

// --- against the store -----------------------------------------------------

/// What [`eval_on`] is asked to measure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRequest {
    pub queries: Vec<EvalQuery>,
    /// A saved seed run to route by; `None` clusters in memory with
    /// [`SeedParams::default`] and saves nothing.
    pub run: Option<String>,
    pub beams: Vec<usize>,
    /// Return the descent records too.
    pub log: bool,
}

/// [`eval_on`]'s answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalOutcome {
    pub report: EvalReport,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub descents: Vec<DescentRecord>,
}

struct Loaded {
    corpus: Corpus,
    snapshot: Option<Snapshot>,
    stored: Option<SeedEmbedding>,
}

/// The corpus: every seedable memory with vectors, priced by its content.
fn load_corpus(store: &Store<'_>) -> Result<Corpus, String> {
    let tokens: HashMap<String, usize> = Memories::new(store)
        .all_live()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(topic_seed::is_seedable)
        .map(|m| (m.id, estimated_content_tokens(&m.content)))
        .collect();
    let rows = topic_seed::memory_vectors(store)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|(id, v)| tokens.get(&id).map(|&t| (id, v, t)))
        .collect();
    Ok(Corpus::new(rows))
}

fn load(store: &Store<'_>, run: Option<&str>) -> Result<Loaded, String> {
    let stored = crate::vectors::read_embedding_meta(store)
        .map_err(|e| e.to_string())?
        .map(|e| SeedEmbedding {
            backend: e.backend,
            model: e.model,
            dim: e.dim,
        });
    let snapshot = match run {
        Some(run) => Some(topic_seed::load(&topic_seed::snapshot_dir(store)?, run)?),
        None => None,
    };
    Ok(Loaded {
        corpus: load_corpus(store)?,
        snapshot,
        stored,
    })
}

fn embed_queries(embedder: &dyn Embedder, queries: &[EvalQuery]) -> Result<Vec<Vec<f32>>, String> {
    let mut out = Vec::with_capacity(queries.len());
    for batch in queries.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|q| q.query.clone()).collect();
        let vectors = embedder
            .embed(&texts, EmbedRole::Query)
            .map_err(|e| format!("embedding the queries failed: {e}"))?;
        if vectors.len() != texts.len() {
            return Err(format!(
                "the embedder returned {} vectors for {} queries",
                vectors.len(),
                texts.len()
            ));
        }
        out.extend(vectors);
    }
    Ok(out)
}

fn check_model(
    stored: Option<&SeedEmbedding>,
    snapshot: Option<&Snapshot>,
    embedder: &dyn Embedder,
) -> Result<(), String> {
    let live = embedder.identity();
    if let Some(s) = stored {
        if (s.backend.as_str(), s.model.as_str(), s.dim)
            != (live.backend.as_str(), live.model.as_str(), live.dim)
        {
            return Err(format!(
                "the stored vectors are {}/{} ({}), the configured embedder is {}/{} ({}); reindex or fix the configuration first",
                s.backend, s.model, s.dim, live.backend, live.model, live.dim
            ));
        }
    }
    if let (Some(s), Some(run)) = (stored, snapshot.and_then(|s| s.embedding.as_ref())) {
        if s != run {
            return Err(format!(
                "seed run {} was clustered from {}/{} ({}), not the stored {}/{} ({}); seed again",
                snapshot.map_or("", |s| s.run.as_str()),
                run.backend,
                run.model,
                run.dim,
                s.backend,
                s.model,
                s.dim
            ));
        }
    }
    Ok(())
}

/// Measure routed against flat search on the live store. The store is read
/// under one handle that is dropped before the queries are embedded (a
/// network call) and measured.
pub fn eval_on(db: &Database, request: &EvalRequest) -> Result<EvalOutcome, String> {
    if request.queries.is_empty() {
        return Err("the eval set has no queries".into());
    }
    let loaded = load(&db.store(), request.run.as_deref())?;
    if loaded.corpus.len() < 2 {
        return Err(format!(
            "routing needs at least 2 embedded memories, found {}",
            loaded.corpus.len()
        ));
    }
    let embedder = crate::embedder::available_embedder()
        .ok_or("no embedder answers; routing eval needs the configured embedding model")?;
    check_model(
        loaded.stored.as_ref(),
        loaded.snapshot.as_ref(),
        embedder.as_ref(),
    )?;
    let vectors = embed_queries(embedder.as_ref(), &request.queries)?;
    if let Some(bad) = vectors.iter().find(|v| v.len() != loaded.corpus.dim()) {
        return Err(format!(
            "query vectors have dimension {}, the stored ones {}",
            bad.len(),
            loaded.corpus.dim()
        ));
    }
    let index = match &loaded.snapshot {
        Some(snapshot) => RouteIndex::from_snapshot(snapshot, &loaded.corpus),
        None => RouteIndex::from_clusters(&topic_seed::cluster_tree(
            &loaded.corpus.vectors,
            &SeedParams::default(),
        )),
    };
    let from = Provenance {
        run: request.run.clone(),
        embedding: loaded.stored,
    };
    let beams: &[usize] = if request.beams.is_empty() {
        &DEFAULT_BEAMS
    } else {
        &request.beams
    };
    let queries: Vec<(EvalQuery, Vec<f32>)> =
        request.queries.iter().cloned().zip(vectors).collect();
    let (report, descents) = evaluate(&index, &loaded.corpus, &queries, beams, &from);
    if report.queries == 0 {
        return Err(format!(
            "none of the {} queries has a relevant memory in this store's corpus; are the labels from another store?",
            request.queries.len()
        ));
    }
    Ok(EvalOutcome {
        report,
        descents: if request.log { descents } else { Vec::new() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topic_seed::{SeedNode, SeedParams};

    fn unit(v: &[f32]) -> Vec<f32> {
        normalize(v.to_vec())
    }

    /// Two well-separated groups on the x and y axes, two members each.
    fn corpus() -> Corpus {
        Corpus::new(vec![
            ("a1".into(), unit(&[1.0, 0.1, 0.0]), 10),
            ("a2".into(), unit(&[1.0, -0.1, 0.0]), 20),
            ("b1".into(), unit(&[0.1, 1.0, 0.0]), 30),
            ("b2".into(), unit(&[-0.1, 1.0, 0.0]), 40),
        ])
    }

    fn node(id: usize, parent: Option<usize>, members: &[&str]) -> SeedNode {
        SeedNode {
            id,
            parent,
            depth: parent.map_or(1, |_| 2),
            size: members.len(),
            members: members.iter().map(|m| m.to_string()).collect(),
            exemplars: Vec::new(),
            label: Some(format!("n{id}")),
            description: None,
        }
    }

    fn snapshot(nodes: Vec<SeedNode>) -> Snapshot {
        Snapshot {
            run: "r1".into(),
            created_at: String::new(),
            embedding: None,
            params: SeedParams::default(),
            nodes,
        }
    }

    /// Top node 0 over leaves 2 (a1) and 3 (a2); leaf 1 holds b1, b2.
    fn two_level() -> Snapshot {
        snapshot(vec![
            node(0, None, &[]),
            node(1, None, &["b1", "b2"]),
            node(2, Some(0), &["a1"]),
            node(3, Some(0), &["a2", "gone"]),
        ])
    }

    #[test]
    fn snapshot_centroids_are_unit_means_of_members_below() {
        let c = corpus();
        let index = RouteIndex::from_snapshot(&two_level(), &c);
        let top = &index.nodes()[0].centroid;
        let expected = unit(&[2.0, 0.0, 0.0]);
        assert!(top.iter().zip(&expected).all(|(a, b)| (a - b).abs() < 1e-5));
        assert_eq!(index.leaves(), 3);
        assert_eq!(index.depth(), 2);
        // "gone" is not in the corpus: skipped, not an error.
        assert_eq!(index.routed(), 4);
    }

    #[test]
    fn beam_one_descends_to_the_nearest_leaf() {
        let c = corpus();
        let index = RouteIndex::from_snapshot(&two_level(), &c);
        let q = unit(&[1.0, 0.2, 0.0]);
        let (ranking, descent) = rank_routed(&index, &c, &q, 1, 10);
        assert_eq!(descent.leaves, vec![2]);
        assert_eq!(descent.levels.len(), 2);
        assert_eq!(ranking.hits, vec![0]);
        // two top centroids, two children, one member
        assert_eq!(descent.centroids_scored, 4);
        assert_eq!(ranking.scored, 5);
    }

    #[test]
    fn a_kept_leaf_competes_with_deeper_children() {
        let c = corpus();
        let index = RouteIndex::from_snapshot(&two_level(), &c);
        let q = unit(&[1.0, 0.9, 0.0]);
        let descent = index.descend(&q, 2);
        // Level 1 keeps both tops; then leaf 1 stays and node 0 opens up.
        assert_eq!(descent.levels[0].len(), 2);
        assert_eq!(descent.leaves.len(), 2);
        assert!(descent.leaves.iter().all(|&l| index.is_leaf(l)));
    }

    #[test]
    fn a_wide_beam_matches_flat_search() {
        let c = corpus();
        let index = RouteIndex::from_snapshot(&two_level(), &c);
        let q = unit(&[0.3, 1.0, 0.2]);
        let flat = rank_flat(&c, &q, 4);
        let (routed, _) = rank_routed(&index, &c, &q, 8, 4);
        assert_eq!(flat.hits, routed.hits);
        assert_eq!(flat.scored, 4);
    }

    #[test]
    fn clusters_route_every_member() {
        let c = corpus();
        let params = SeedParams {
            top_k: 2,
            leaf_max: 1,
            split_per: 1,
            ..SeedParams::default()
        };
        let index = RouteIndex::from_clusters(&topic_seed::cluster_tree(&c.vectors, &params));
        assert_eq!(index.routed(), c.len());
        let (ranking, _) = rank_routed(&index, &c, &unit(&[0.0, 1.0, 0.0]), 1, 10);
        assert!(ranking.hits.iter().all(|&i| c.ids[i].starts_with('b')));
    }

    #[test]
    fn a_node_without_members_in_the_corpus_is_never_chosen() {
        let c = corpus();
        let snap = snapshot(vec![node(0, None, &["gone"]), node(1, None, &["a1"])]);
        let index = RouteIndex::from_snapshot(&snap, &c);
        let descent = index.descend(&unit(&[0.0, 1.0, 0.0]), 2);
        assert_eq!(descent.leaves, vec![1]);
    }

    #[test]
    fn a_leaf_whose_vectors_cancel_is_still_routable() {
        let c = Corpus::new(vec![
            ("p".into(), vec![1.0, 0.0], 1),
            ("n".into(), vec![-1.0, 0.0], 1),
        ]);
        let index = RouteIndex::from_snapshot(&snapshot(vec![node(0, None, &["p", "n"])]), &c);
        assert_eq!(index.nodes()[0].centroid.len(), 2);
        let (ranking, _) = rank_routed(&index, &c, &[1.0, 0.0], 1, 10);
        assert_eq!(ranking.hits.len(), 2);
    }

    #[test]
    fn a_run_with_no_measured_query_is_never_a_go() {
        let c = corpus();
        let index = RouteIndex::from_snapshot(&two_level(), &c);
        let only = EvalQuery {
            query: "elsewhere".into(),
            relevant: vec!["missing".into()],
            kind: None,
        };
        let (report, _) = evaluate(
            &index,
            &c,
            &[(only, unit(&[1.0, 0.0, 0.0]))],
            &[1],
            &Provenance::default(),
        );
        assert_eq!(report.queries, 0);
        assert!(!report.routed[0].verdict.go);
    }

    #[test]
    fn metrics_handle_short_lists_and_empty_input() {
        assert_eq!(recall_at(&[1, 2], &[2, 9], 10), 0.5);
        assert_eq!(recall_at(&[1, 2], &[2], 1), 0.0);
        assert_eq!(recall_at(&[1], &[], 5), 0.0);
        assert_eq!(tokens_at(&[0, 3], &corpus(), 10), 50);
        assert_eq!(percentile(&[], 95.0), 0.0);
        assert_eq!(percentile(&[3.0, 1.0, 2.0, 4.0], 50.0), 2.0);
        assert_eq!(percentile(&[3.0, 1.0, 2.0, 4.0], 95.0), 4.0);
    }

    fn path(recall: &[f64], tokens: &[f64], p95: f64) -> PathScores {
        let pad = |v: &[f64]| {
            let mut v = v.to_vec();
            v.resize(MAX_K, *v.last().unwrap_or(&0.0));
            v
        };
        let recall_curve = pad(recall);
        let tokens_curve = pad(tokens);
        PathScores {
            recall_at_5: recall_curve[4],
            recall_at_10: recall_curve[9],
            tokens_at_5: tokens_curve[4],
            tokens_at_10: tokens_curve[9],
            vectors_scored: 0.0,
            latency_p50_us: 0.0,
            latency_p95_us: p95,
            recall_curve,
            tokens_curve,
        }
    }

    #[test]
    fn the_go_rule_needs_recall_tokens_and_latency() {
        let ks: Vec<f64> = (1..=10).map(|k| k as f64 * 100.0).collect();
        let flat = path(
            &[0.5, 0.6, 0.7, 0.8, 0.8, 0.8, 0.8, 0.8, 0.8, 0.8],
            &ks,
            100.0,
        );
        // Reaches 0.78 at k = 4: 400 tokens against flat's 1,000.
        let good = path(
            &[0.5, 0.6, 0.7, 0.79, 0.79, 0.79, 0.79, 0.79, 0.79, 0.79],
            &ks,
            150.0,
        );
        let m = tokens_to_match(&flat, &good);
        assert_eq!(
            m,
            Some(Match {
                k: 4,
                tokens: 400.0
            })
        );
        assert!(verdict(&flat, &good, m).go);

        let slow = path(
            &[0.5, 0.6, 0.7, 0.79, 0.79, 0.79, 0.79, 0.79, 0.79, 0.79],
            &ks,
            201.0,
        );
        let v = verdict(&flat, &slow, tokens_to_match(&flat, &slow));
        assert!(!v.latency_ok && !v.go);

        let lossy = path(
            &[0.5, 0.6, 0.7, 0.7, 0.7, 0.7, 0.7, 0.7, 0.7, 0.7],
            &ks,
            50.0,
        );
        let m = tokens_to_match(&flat, &lossy);
        assert_eq!(m, None);
        let v = verdict(&flat, &lossy, m);
        assert!(!v.recall_ok && !v.tokens_ok);
    }

    #[test]
    fn evaluate_skips_unlabelled_queries_and_logs_each_descent() {
        let c = corpus();
        let index = RouteIndex::from_snapshot(&two_level(), &c);
        let q = |text: &str, relevant: &[&str]| EvalQuery {
            query: text.into(),
            relevant: relevant.iter().map(|r| r.to_string()).collect(),
            kind: None,
        };
        let queries = vec![
            (q("about a", &["a1", "a2"]), unit(&[1.0, 0.0, 0.0])),
            (q("about b", &["b2", "missing"]), unit(&[0.0, 1.0, 0.0])),
            (q("nothing", &["missing"]), unit(&[0.0, 0.0, 1.0])),
        ];
        let from = Provenance {
            run: Some("r1".into()),
            embedding: None,
        };
        let (report, descents) = evaluate(&index, &c, &queries, &[1, 4], &from);
        assert_eq!(report.queries, 2);
        assert_eq!(report.skipped, vec!["nothing".to_string()]);
        assert_eq!(report.missing_relevant, 2);
        assert_eq!(report.flat.recall_at_10, 1.0);
        assert_eq!(report.routed.len(), 2);
        assert_eq!(report.routed[1].scores.recall_at_10, 1.0);
        assert_eq!(descents.len(), 4);
        assert_eq!(descents[0].run.as_deref(), Some("r1"));
        assert_eq!(descents[0].path[0][0].1.as_deref(), Some("n0"));
        assert_eq!(descents[0].hits.len(), descents[0].top.len());
    }
}
