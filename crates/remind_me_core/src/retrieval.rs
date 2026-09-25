use crate::models::{Memory, MemorySearchResult, RetrievalStrategy};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Configuration (REMIND_ME_RRF_* environment variables)
// ---------------------------------------------------------------------------
//
// Mirrors `remind_me_mcp/retrieval.py`'s module-level `RRF_K` / `RRF_W_*` /
// `RRF_FUSION` constants, but reads each env var fresh at call time rather
// than once at process start — this crate's established convention (e.g.
// `webhook::WebhookConfig::from_env`, `remote::RemoteConfig::from_env`) so
// tests can set/unset a var per case instead of needing a process restart.

const ENV_RRF_K: &str = "REMIND_ME_RRF_K";
const ENV_RRF_W_KEYWORD: &str = "REMIND_ME_RRF_W_KEYWORD";
const ENV_RRF_W_SEMANTIC: &str = "REMIND_ME_RRF_W_SEMANTIC";
const ENV_RRF_W_RECENCY: &str = "REMIND_ME_RRF_W_RECENCY";
const ENV_RRF_W_VITALITY: &str = "REMIND_ME_RRF_W_VITALITY";
const ENV_RRF_W_IDF: &str = "REMIND_ME_RRF_W_IDF";
const ENV_RRF_FUSION: &str = "REMIND_ME_RRF_FUSION";

/// Reciprocal Rank Fusion smoothing constant. Higher values produce more
/// uniform scores. Matches the reference's `RRF_K` default.
pub const RRF_K_DEFAULT: f64 = 60.0;

/// Parse an env var as a finite `f64`, falling back to `default` when the
/// variable is unset, unparseable, or non-finite (NaN/±infinity).
///
/// The reference parses these with a bare `float(os.environ.get(...))` —
/// unparseable input raises an uncaught `ValueError` that crashes the whole
/// process at import time, before it ever serves a request. That is a
/// meaningfully different failure surface from this port's read-at-call-time
/// convention: reading fresh on every search means a literal translation
/// would crash on every subsequent call forever, not fail once at boot the
/// way the reference does. Rather than adopt a worse failure mode, this
/// falls back to the documented default — the same graceful-degradation
/// spirit this codebase already applies to tool-call parameters (e.g.
/// `EntityTraverseInput::hops`), extended here to process-level config too.
fn env_f64(var: &str, default: f64) -> f64 {
    std::env::var(var)
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

/// Read `REMIND_ME_RRF_K` fresh from the environment, falling back to
/// [`RRF_K_DEFAULT`].
pub fn rrf_k_from_env() -> f64 {
    env_f64(ENV_RRF_K, RRF_K_DEFAULT)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RrfWeights {
    pub w_keyword: f64,
    pub w_semantic: f64,
    pub w_recency: f64,
    pub w_vitality: f64,
    pub w_idf: f64,
}

impl Default for RrfWeights {
    /// The reference's own defaults: the original four signals at 1.0, and
    /// `idf` at 0.0 since it's a newer, opt-in lever layered on top of
    /// already-tuned defaults — flipping it on by default would silently
    /// shift existing search behavior for anyone who hasn't opted in.
    fn default() -> Self {
        Self {
            w_keyword: 1.0,
            w_semantic: 1.0,
            w_recency: 1.0,
            w_vitality: 1.0,
            w_idf: 0.0,
        }
    }
}

impl RrfWeights {
    /// Read the five `REMIND_ME_RRF_W_*` weights fresh from the environment.
    /// Each falls back to [`RrfWeights::default`]'s corresponding field when
    /// unset, unparseable, or non-finite — see [`env_f64`] for why this
    /// degrades gracefully rather than mirroring the reference's hard crash.
    ///
    /// Unlike the reference, out-of-range magnitudes (negative, huge) are
    /// *not* clamped: the reference itself never bounds these, and a
    /// negative weight is a legitimate (if unusual) way to ask a signal to
    /// actively penalize a rank, not an error to correct silently.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            w_keyword: env_f64(ENV_RRF_W_KEYWORD, default.w_keyword),
            w_semantic: env_f64(ENV_RRF_W_SEMANTIC, default.w_semantic),
            w_recency: env_f64(ENV_RRF_W_RECENCY, default.w_recency),
            w_vitality: env_f64(ENV_RRF_W_VITALITY, default.w_vitality),
            w_idf: env_f64(ENV_RRF_W_IDF, default.w_idf),
        }
    }

    /// Scale every field by a [`StrategyMultipliers`] preset. A multiplier
    /// composes with (rather than replaces) whatever this base already is —
    /// so a base weight the env vars zeroed out (e.g.
    /// `REMIND_ME_RRF_W_IDF=0`) stays zero under any preset, since
    /// `0.0 * anything == 0.0`.
    fn scaled_by(self, m: StrategyMultipliers) -> Self {
        Self {
            w_keyword: self.w_keyword * m.w_keyword,
            w_semantic: self.w_semantic * m.w_semantic,
            w_recency: self.w_recency * m.w_recency,
            w_vitality: self.w_vitality * m.w_vitality,
            w_idf: self.w_idf * m.w_idf,
        }
    }
}

/// `REMIND_ME_RRF_FUSION`: which formula [`rank_rrf`] combines signals with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RrfFusion {
    /// Classic reciprocal rank fusion: `sum(weight / (k + rank))` over each
    /// signal's *ordinal position*.
    #[default]
    Rank,
    /// Normalized-magnitude fusion: `sum(weight * normalized_score)` over
    /// each signal's underlying value, min-max normalized to `[0, 1]`.
    Score,
}

impl RrfFusion {
    /// Any value other than exactly `"score"` (case-insensitive) is treated
    /// as `"rank"` — matching the reference's own `if fusion == "score":
    /// ... else: ...`, which never validates or rejects the string; an
    /// unrecognized value (or none at all) silently gets the default
    /// behavior rather than an error.
    pub fn from_env() -> Self {
        match std::env::var(ENV_RRF_FUSION) {
            Ok(v) if v.trim().eq_ignore_ascii_case("score") => Self::Score,
            _ => Self::Rank,
        }
    }
}

/// The RRF smoothing constant, per-signal weights, and fusion mode, resolved
/// together — what the reference calls a "retrieval-quality profile"
/// (`benchmarks/runner.py --rrf-profile`).
#[derive(Debug, Clone, Copy)]
pub struct RrfConfig {
    pub k: f64,
    pub weights: RrfWeights,
    pub fusion: RrfFusion,
}

impl Default for RrfConfig {
    fn default() -> Self {
        Self {
            k: RRF_K_DEFAULT,
            weights: RrfWeights::default(),
            fusion: RrfFusion::Rank,
        }
    }
}

/// Raw per-signal magnitudes keyed by memory id, used only by
/// [`RrfFusion::Score`] mode; ignored entirely in [`RrfFusion::Rank`] mode.
/// `rank_rrf` never touches the database itself, so callers gather these
/// from whichever tier produced them.
#[derive(Debug, Clone, Default)]
pub struct RrfSignals {
    /// FTS5 `bm25()` score per memory id that had a keyword hit — lower is a
    /// better match, SQLite's own convention. Absent for a semantic-only
    /// hit.
    pub keyword_bm25: HashMap<String, f64>,
    /// Cosine similarity per memory id that had a semantic hit — higher is
    /// a better match. Absent for a keyword-only hit.
    pub semantic_similarity: HashMap<String, f64>,
}

// ---------------------------------------------------------------------------
// Strategy-preset auto-adjustment
// ---------------------------------------------------------------------------

/// Multipliers applied on top of the live base weights ([`RrfWeights::from_env`]
/// composed with whatever [`choose_rrf_weights`]'s caller already resolved),
/// not fixed absolute numbers — a preset never resurrects a signal the base
/// config deliberately zeroed out.
#[derive(Debug, Clone, Copy)]
struct StrategyMultipliers {
    w_keyword: f64,
    w_semantic: f64,
    w_recency: f64,
    w_vitality: f64,
    w_idf: f64,
}

impl StrategyMultipliers {
    const IDENTITY: Self = Self {
        w_keyword: 1.0,
        w_semantic: 1.0,
        w_recency: 1.0,
        w_vitality: 1.0,
        w_idf: 1.0,
    };

    /// Quoted phrases, prefix* wildcards, and short/structured queries are
    /// exact-match-shaped — lean on keyword relevance. Semantic isn't
    /// dropped to 0: even a keyword-shaped query can have a semantically
    /// relevant hit worth surfacing, just weighted lower.
    const KEYWORD_FAVORED: Self = Self {
        w_keyword: 1.5,
        w_semantic: 0.5,
        ..Self::IDENTITY
    };

    /// Long, natural-language, question-shaped queries rarely share exact
    /// terms with the memory they're looking for — lean on semantic
    /// similarity.
    const SEMANTIC_FAVORED: Self = Self {
        w_keyword: 0.5,
        w_semantic: 1.5,
        ..Self::IDENTITY
    };
}

pub fn looks_keyword_shaped(query: &str) -> bool {
    query.contains('"') || query.contains('*') || query.split_whitespace().count() <= 2
}

pub fn looks_semantic_shaped(query: &str) -> bool {
    query.split_whitespace().count() >= 6 || query.trim().ends_with('?')
}

pub fn looks_temporal_shaped(query: &str) -> bool {
    let lower = query.to_lowercase();
    let keywords = [
        "before",
        "after",
        "since",
        "until",
        "when",
        "during",
        "ago",
        "recently",
        "yesterday",
        "today",
        "tomorrow",
        "last week",
        "last month",
        "last year",
    ];
    keywords.iter().any(|kw| lower.contains(kw))
}

/// Boosts `w_recency` on top of whichever profile the query shape otherwise
/// picked — only recency moves, so this composes with either profile
/// instead of replacing it.
const TEMPORAL_RECENCY_MULTIPLIER: f64 = 1.5;

/// Heuristically route a query to an RRF weight profile, composed on top of
/// the env-configured base weights ([`RrfWeights::from_env`]).
///
/// `strategy` pins an explicit profile (`Balanced`/`KeywordFavored`/
/// `SemanticFavored`); `Auto` instead routes by the query's observable shape,
/// the same heuristic the reference's own `choose_rrf_weights` uses. Either
/// way, a temporal expression ("last summer", "before I moved") additionally
/// boosts `w_recency` by [`TEMPORAL_RECENCY_MULTIPLIER`] on top of whatever
/// profile was selected.
pub fn choose_rrf_weights(query: &str, strategy: RetrievalStrategy) -> RrfWeights {
    let base = RrfWeights::from_env();

    let mut weights = match strategy {
        RetrievalStrategy::Balanced => base.scaled_by(StrategyMultipliers::IDENTITY),
        RetrievalStrategy::KeywordFavored => base.scaled_by(StrategyMultipliers::KEYWORD_FAVORED),
        RetrievalStrategy::SemanticFavored => base.scaled_by(StrategyMultipliers::SEMANTIC_FAVORED),
        RetrievalStrategy::Auto => {
            if looks_keyword_shaped(query) {
                base.scaled_by(StrategyMultipliers::KEYWORD_FAVORED)
            } else if looks_semantic_shaped(query) {
                base.scaled_by(StrategyMultipliers::SEMANTIC_FAVORED)
            } else {
                base.scaled_by(StrategyMultipliers::IDENTITY)
            }
        }
    };

    if looks_temporal_shaped(query) {
        weights.w_recency *= TEMPORAL_RECENCY_MULTIPLIER;
    }

    weights
}

// ---------------------------------------------------------------------------
// RRF ranking
// ---------------------------------------------------------------------------

/// Min-max normalize a `{id: raw_value}` map to `[0, 1]`, higher = better.
///
/// `invert` is true when a *lower* raw value is better (bm25), so the
/// normalized score is flipped to keep "higher = better" uniform across
/// every signal. When every value ties (including a single value), every id
/// gets a score of 1.0 — there's no meaningful spread to normalize, and 1.0
/// (rather than 0.0) avoids silently zeroing out a signal that's simply
/// uniform.
fn minmax_normalize(raw: &HashMap<String, f64>, invert: bool) -> HashMap<String, f64> {
    if raw.is_empty() {
        return HashMap::new();
    }
    let lo = raw.values().copied().fold(f64::INFINITY, f64::min);
    let hi = raw.values().copied().fold(f64::NEG_INFINITY, f64::max);
    if hi - lo < 1e-12 {
        return raw.keys().map(|id| (id.clone(), 1.0)).collect();
    }
    raw.iter()
        .map(|(id, v)| {
            let n = (v - lo) / (hi - lo);
            (id.clone(), if invert { 1.0 - n } else { n })
        })
        .collect()
}

/// Parse `created_at` into a Unix timestamp for score-mode recency
/// normalization. A missing or unparseable value is treated as the epoch
/// (the oldest possible value) — matching the reference, which does the same
/// on a `ValueError`/`TypeError` from `datetime.fromisoformat`.
fn recency_epoch(created_at: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|dt| dt.timestamp() as f64)
        .unwrap_or(0.0)
}

/// Fuse a keyword-ranked list and a semantic-ranked list via [`RrfConfig::fusion`].
///
/// Every result carries five signals: keyword, semantic, recency, vitality,
/// and IDF (`w_idf`, off by default — see [`RrfWeights::default`]).
///
/// - [`RrfFusion::Rank`] (default): each signal contributes `weight / (k +
///   rank)`, where `rank` is that memory's 1-indexed position once the
///   candidate pool is ordered by the signal (keyword/semantic use the
///   caller-supplied list order directly; recency/vitality/IDF are ranked by
///   sorting the deduplicated union by `created_at` descending, `vitality`
///   descending, and `bm25` ascending respectively). A memory missing from
///   the keyword or semantic list gets a *penalty rank* one past that list's
///   length — the same treatment recency/vitality/IDF give a memory with no
///   signal at all (e.g. no `created_at`), and consistent with the
///   reference's own reciprocal-rank-fusion.
/// - [`RrfFusion::Score`] (issue #49 in the reference): min-max normalizes
///   the real underlying magnitudes (`RrfSignals`, `created_at`, `vitality`)
///   into `[0, 1]` and sums `weight * normalized_score`. A memory missing a
///   signal gets 0.0 for it — the worst possible score, mirroring rank
///   mode's penalty-rank treatment. `w_idf` reuses the keyword signal's
///   normalized magnitude in this mode, since both derive from the same
///   underlying `bm25` value — there is no separate IDF magnitude to
///   normalize once magnitude, not just position, is in play.
///
/// A memory present in only one of `keyword`/`semantic` is not penalized to
/// zero on the other. Before `#49`, `semantic` was always empty (nothing
/// produced vectors) — that's still exactly the single-list case this
/// handles: every candidate's semantic rank was the constant penalty, so
/// `w_semantic` contributed a constant that dropped out of the ordering
/// entirely — silently correct, never exercised. `vec_score` (and this
/// signal's contribution to `score`) is `None` precisely when `semantic` is
/// empty, distinguishing "semantic never ran" from "ran and found nothing
/// relevant here."
pub fn rank_rrf(
    keyword: Vec<Memory>,
    semantic: Vec<Memory>,
    config: RrfConfig,
    signals: &RrfSignals,
) -> Vec<MemorySearchResult> {
    let keyword_rank: HashMap<String, usize> = keyword
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i + 1))
        .collect();
    let semantic_rank: HashMap<String, usize> = semantic
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i + 1))
        .collect();
    let keyword_penalty = (keyword.len() + 1) as f64;
    let semantic_penalty = (semantic.len() + 1) as f64;
    // Distinct from "semantic search ran and found nothing at this rank": an
    // empty `semantic` means it never ran at all (no embedder configured or
    // reachable), so `vec_score` reports `None` rather than the same
    // constant for every result, which would carry no information but look
    // like it did.
    let semantic_ran = !semantic.is_empty();

    // Union, keyword-first, deduplicated by id, so a memory in both lists
    // appears once.
    let mut seen: HashSet<String> = HashSet::new();
    let union: Vec<Memory> = keyword
        .into_iter()
        .chain(semantic)
        .filter(|m| seen.insert(m.id.clone()))
        .collect();

    if union.is_empty() {
        return Vec::new();
    }

    // Recency ranking: sort all unique memories by created_at DESC. String
    // comparison, not a parsed timestamp — matching the reference, and
    // correct as long as `created_at` is a consistently-formatted ISO 8601
    // string, which it always is here.
    let mut by_recency: Vec<&Memory> = union.iter().collect();
    by_recency.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let recency_rank: HashMap<String, usize> = by_recency
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i + 1))
        .collect();

    // Vitality ranking: sort all unique memories by vitality DESC.
    let mut by_vitality: Vec<&Memory> = union.iter().collect();
    by_vitality.sort_by(|a, b| {
        b.vitality
            .partial_cmp(&a.vitality)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let vitality_rank: HashMap<String, usize> = by_vitality
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i + 1))
        .collect();

    // IDF ranking: sort all unique memories by bm25 ASCENDING (lower =
    // better match). Memories with no FTS hit (semantic-only) have no bm25
    // value and sort last, via the +infinity default.
    let mut by_idf: Vec<&Memory> = union.iter().collect();
    by_idf.sort_by(|a, b| {
        let av = signals
            .keyword_bm25
            .get(&a.id)
            .copied()
            .unwrap_or(f64::INFINITY);
        let bv = signals
            .keyword_bm25
            .get(&b.id)
            .copied()
            .unwrap_or(f64::INFINITY);
        av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
    });
    let idf_rank: HashMap<String, usize> = by_idf
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i + 1))
        .collect();

    // Score-mode normalized magnitudes. Computed once up front; left empty
    // (and unused) in rank mode.
    let (keyword_score, semantic_score, recency_score, vitality_score) =
        if config.fusion == RrfFusion::Score {
            let recency_raw: HashMap<String, f64> = union
                .iter()
                .map(|m| (m.id.clone(), recency_epoch(&m.created_at)))
                .collect();
            let vitality_raw: HashMap<String, f64> =
                union.iter().map(|m| (m.id.clone(), m.vitality)).collect();
            (
                minmax_normalize(&signals.keyword_bm25, true),
                minmax_normalize(&signals.semantic_similarity, false),
                minmax_normalize(&recency_raw, false),
                minmax_normalize(&vitality_raw, false),
            )
        } else {
            (
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )
        };

    let w = config.weights;
    let k = config.k;

    let mut ranked: Vec<MemorySearchResult> = union
        .into_iter()
        .map(|memory| {
            let kw_rank = keyword_rank
                .get(memory.id.as_str())
                .map(|&r| r as f64)
                .unwrap_or(keyword_penalty);
            let sem_rank = semantic_rank
                .get(memory.id.as_str())
                .map(|&r| r as f64)
                .unwrap_or(semantic_penalty);
            let rec_rank = recency_rank[&memory.id] as f64;
            let vit_rank = vitality_rank[&memory.id] as f64;
            let idf_r = idf_rank[&memory.id] as f64;

            let (fts_score, vec_score, recency_contrib, vitality_contrib, idf_contrib) =
                match config.fusion {
                    RrfFusion::Rank => (
                        w.w_keyword / (k + kw_rank),
                        w.w_semantic / (k + sem_rank),
                        w.w_recency / (k + rec_rank),
                        w.w_vitality / (k + vit_rank),
                        w.w_idf / (k + idf_r),
                    ),
                    RrfFusion::Score => {
                        let ks = keyword_score.get(&memory.id).copied().unwrap_or(0.0);
                        let ss = semantic_score.get(&memory.id).copied().unwrap_or(0.0);
                        let rs = recency_score.get(&memory.id).copied().unwrap_or(0.0);
                        let vs = vitality_score.get(&memory.id).copied().unwrap_or(0.0);
                        (
                            w.w_keyword * ks,
                            w.w_semantic * ss,
                            w.w_recency * rs,
                            w.w_vitality * vs,
                            // Reuses the keyword signal's normalized magnitude
                            // -- same underlying bm25 value, see doc comment.
                            w.w_idf * ks,
                        )
                    }
                };

            let total = fts_score
                + if semantic_ran { vec_score } else { 0.0 }
                + recency_contrib
                + vitality_contrib
                + idf_contrib;

            MemorySearchResult {
                memory,
                score: total,
                fts_score: Some(fts_score),
                vec_score: semantic_ran.then_some(vec_score),
                recency_score: Some(recency_contrib),
                vitality_score: Some(vitality_contrib),
                idf_score: Some(idf_contrib),
                // Feedback is applied in a later pipeline stage
                // (`vitality::apply_feedback_adjustment`), which needs a
                // database connection this pure ranking function doesn't
                // have.
                feedback_adjustment: None,
                // Likewise applied later, by `reranker::maybe_rerank`, which
                // reorders the head rather than contributing to this total.
                rerank_score: None,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

/// What a token-budget trim kept, and what it cost.
///
/// The counts were always computed and then discarded, so a search that
/// dropped half its results looked identical to one that returned everything
/// (#200). Reporting them is the difference between "this is everything that
/// matched" being a safe inference and a wrong one.
#[derive(Debug, Clone)]
pub struct TrimOutcome {
    pub results: Vec<MemorySearchResult>,
    /// How many were ranked before the budget was applied.
    pub total_candidates: usize,
    /// How many survived it — `results.len()`, named for the response.
    pub returned: usize,
    /// How many were dropped. A **count**, matching the reference, not a
    /// boolean: "3 were cut" and "something was cut" are different answers,
    /// and only one of them tells a caller whether to raise the budget.
    pub trimmed: usize,
    /// Estimated tokens across what was kept.
    pub tokens_used: usize,
    /// What the wall-clock deadline did to this search (#257).
    ///
    /// Default (no deadline, nothing skipped) for a bare trim; the search path
    /// fills it in. It rides here because [`TrimOutcome`] is already the one
    /// channel out of `search_memories_budgeted`, and a second return value
    /// would have to be threaded through three public signatures to say one
    /// thing.
    pub timing: SearchTiming,
}

/// Wall-clock budget for one search.
pub const SEARCH_DEADLINE_ENV: &str = "REMIND_ME_SEARCH_DEADLINE_MS";

/// What the deadline did to a search.
///
/// Separate from the trim envelope because they answer different questions: a
/// trimmed search returned less than it *found*, a timed-out one returned less
/// than it would have *looked for*. Reporting the second as the first would
/// tell a caller to raise `token_budget` when the fix is a longer deadline —
/// or a reachable embedder.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchTiming {
    /// Wall-clock spent in the search.
    pub elapsed_ms: u64,
    /// The deadline in force, if any. `None` means unbounded — today's
    /// behaviour, and still the default.
    pub deadline_ms: Option<u64>,
    /// Stages not run because the deadline had already passed, in the order
    /// they would have run.
    ///
    /// A **list, not a boolean**, for the reason `trimmed` is a count: "we
    /// skipped the semantic stage" and "we skipped semantic and rerank" call
    /// for different responses from a caller deciding whether to trust the
    /// result set.
    pub skipped: Vec<String>,
}

impl SearchTiming {
    /// Whether the deadline changed what this search did.
    pub fn degraded(&self) -> bool {
        !self.skipped.is_empty()
    }
}

/// A wall-clock budget, checked between stages.
///
/// # What this does and does not bound
///
/// It gates **stage entry**: before starting the semantic stage or the
/// reranker, the search asks whether the deadline has passed and skips the
/// stage if so. It cannot preempt work already running — a socket read inside
/// the embedder is bounded by that embedder's own `IO_TIMEOUT` (60s at the
/// time of writing), not by this. So the worst case is the deadline *plus* one
/// in-flight stage, not the deadline.
///
/// That is a real limit and the reason this is not called a timeout. What it
/// buys is that a slow stage cannot *compound*: a search that spent 60s
/// failing to reach Ollama will not then spend more on a cross-encoder rerank
/// nobody is waiting for any more. Bounding the blocking call itself means
/// deriving the socket timeout from the remaining budget, which needs the
/// deadline threaded into [`crate::embedder::Embedder`] — a public signature
/// change, and a separate piece of work.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    started: Instant,
    limit: Option<Duration>,
}

impl Deadline {
    /// Start a deadline from `REMIND_ME_SEARCH_DEADLINE_MS`.
    ///
    /// Read fresh at call time rather than cached, matching the
    /// `REMIND_ME_RRF_*` vars above. Unset, zero, or unparseable all mean
    /// **unbounded**: this sits on the search hot path, and refusing to search
    /// because a tuning knob is malformed trades a small misconfiguration for
    /// an outage. Zero is treated as unset rather than as "expire
    /// immediately", since a zero-length deadline would silently reduce every
    /// search to keyword-only.
    pub fn from_env() -> Self {
        let limit = std::env::var(SEARCH_DEADLINE_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis);
        Self {
            started: Instant::now(),
            limit,
        }
    }

    /// An explicitly unbounded deadline, for callers that never want one.
    pub fn unbounded() -> Self {
        Self {
            started: Instant::now(),
            limit: None,
        }
    }

    /// A deadline of `limit` starting now.
    ///
    /// Lets a caller set its own budget instead of inheriting the global one —
    /// a background poll can afford longer than an interactive search — and,
    /// as with [`crate::db::queries::search_memories_with_embedder`], it is
    /// what makes the behaviour testable at all. A deadline read from the
    /// environment starts its clock when the search starts, so from outside
    /// there is no way to arrange for it to have passed except by making the
    /// search itself slow, which is a race rather than a test.
    pub fn starting_now(limit: Option<Duration>) -> Self {
        Self {
            started: Instant::now(),
            limit,
        }
    }

    /// A deadline that has already passed.
    ///
    /// `Duration::ZERO` rather than a `from_env` value, deliberately: env
    /// parsing treats `0` as *unset* so nobody configures an accidental
    /// keyword-only mode, but a caller (or a test) asking for this by name has
    /// unambiguously said what they mean.
    pub fn already_passed() -> Self {
        Self {
            started: Instant::now(),
            limit: Some(Duration::ZERO),
        }
    }

    pub fn expired(&self) -> bool {
        match self.limit {
            Some(limit) => self.started.elapsed() >= limit,
            None => false,
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn limit_ms(&self) -> Option<u64> {
        self.limit.map(|d| d.as_millis() as u64)
    }
}

/// Estimated token cost of a piece of content.
///
/// `.max(1)` is this crate's existing behaviour and is deliberately kept: the
/// reference uses a bare `len / 4`, which estimates zero for content under
/// four characters, so an unbounded number of very short memories can enter a
/// budgeted response. That divergence changes *which memories come back*, not
/// just the reported number, so it is left alone here rather than folded into
/// a reporting fix — see the note in #200.
///
/// Public because the L2/L3 bootstrap ([`crate::promotion::bootstrap`], #255)
/// spends from the same `token_budget` these results are trimmed against. Two
/// estimators over one budget would let the reserve and the remainder disagree
/// about how much was actually spent, and the discrepancy would show up as a
/// response slightly over budget rather than as an obvious fault.
pub fn estimated_content_tokens(content: &str) -> usize {
    (content.len() / 4).max(1)
}

/// Estimated token cost of one result.
fn estimated_tokens(result: &MemorySearchResult) -> usize {
    estimated_content_tokens(&result.memory.content)
}

pub fn trim_by_token_budget(results: Vec<MemorySearchResult>, token_budget: usize) -> TrimOutcome {
    let total_candidates = results.len();

    if token_budget == 0 {
        // Unlimited. The tokens are still counted, because "how big was this
        // response" is a fair question even when nothing was cut.
        let tokens_used = results.iter().map(estimated_tokens).sum();
        return TrimOutcome {
            returned: total_candidates,
            trimmed: 0,
            tokens_used,
            total_candidates,
            results,
            timing: SearchTiming::default(),
        };
    }

    let mut tokens_used = 0;
    let mut kept = Vec::new();

    for res in results {
        let est_tokens = estimated_tokens(&res);
        if tokens_used + est_tokens > token_budget && !kept.is_empty() {
            break;
        }
        tokens_used += est_tokens;
        kept.push(res);
    }

    TrimOutcome {
        returned: kept.len(),
        trimmed: total_candidates - kept.len(),
        tokens_used,
        total_candidates,
        results: kept,
        timing: SearchTiming::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global; serialize this module's env-touching
    // tests so they don't race each other the way `cargo test` otherwise
    // would.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        crate::test_env::remove_var(ENV_RRF_K);
        crate::test_env::remove_var(ENV_RRF_W_KEYWORD);
        crate::test_env::remove_var(ENV_RRF_W_SEMANTIC);
        crate::test_env::remove_var(ENV_RRF_W_RECENCY);
        crate::test_env::remove_var(ENV_RRF_W_VITALITY);
        crate::test_env::remove_var(ENV_RRF_W_IDF);
        crate::test_env::remove_var(ENV_RRF_FUSION);
    }

    #[test]
    fn test_choose_rrf_weights_auto() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let w = choose_rrf_weights("what did I do last week?", RetrievalStrategy::Auto);
        assert!(w.w_recency > 1.0);
        clear_env();
    }

    #[test]
    fn rrf_k_defaults_to_sixty_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert_eq!(rrf_k_from_env(), RRF_K_DEFAULT);
        clear_env();
    }

    #[test]
    fn rrf_k_reads_a_valid_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(ENV_RRF_K, "12.5");
        assert_eq!(rrf_k_from_env(), 12.5);
        clear_env();
    }

    #[test]
    fn rrf_k_falls_back_to_default_on_an_unparseable_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(ENV_RRF_K, "not-a-number");
        assert_eq!(rrf_k_from_env(), RRF_K_DEFAULT);
        clear_env();
    }

    #[test]
    fn rrf_k_falls_back_to_default_on_a_non_finite_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(ENV_RRF_K, "NaN");
        assert_eq!(rrf_k_from_env(), RRF_K_DEFAULT);
        crate::test_env::set_var(ENV_RRF_K, "inf");
        assert_eq!(rrf_k_from_env(), RRF_K_DEFAULT);
        clear_env();
    }

    #[test]
    fn rrf_weights_from_env_reads_all_five_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(ENV_RRF_W_KEYWORD, "2.0");
        crate::test_env::set_var(ENV_RRF_W_SEMANTIC, "0.25");
        crate::test_env::set_var(ENV_RRF_W_RECENCY, "0.0");
        crate::test_env::set_var(ENV_RRF_W_VITALITY, "3.0");
        crate::test_env::set_var(ENV_RRF_W_IDF, "1.0");

        let w = RrfWeights::from_env();

        assert_eq!(w.w_keyword, 2.0);
        assert_eq!(w.w_semantic, 0.25);
        assert_eq!(w.w_recency, 0.0);
        assert_eq!(w.w_vitality, 3.0);
        assert_eq!(w.w_idf, 1.0);
        clear_env();
    }

    #[test]
    fn rrf_weights_from_env_matches_the_reference_defaults_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let w = RrfWeights::from_env();
        assert_eq!(w.w_keyword, 1.0);
        assert_eq!(w.w_semantic, 1.0);
        assert_eq!(w.w_recency, 1.0);
        assert_eq!(w.w_vitality, 1.0);
        assert_eq!(w.w_idf, 0.0);
        clear_env();
    }

    #[test]
    fn a_malformed_weight_falls_back_to_its_own_default_not_the_whole_profile() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(ENV_RRF_W_KEYWORD, "garbage");
        crate::test_env::set_var(ENV_RRF_W_SEMANTIC, "0.75");

        let w = RrfWeights::from_env();

        assert_eq!(w.w_keyword, 1.0, "malformed value falls back to default");
        assert_eq!(w.w_semantic, 0.75, "a sibling override is unaffected");
        clear_env();
    }

    #[test]
    fn rrf_weights_from_env_accepts_an_out_of_range_negative_weight_unclamped() {
        // The reference never bounds these -- a negative weight is a
        // deliberate way to penalize a signal, not malformed input.
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(ENV_RRF_W_KEYWORD, "-2.5");
        assert_eq!(RrfWeights::from_env().w_keyword, -2.5);
        clear_env();
    }

    #[test]
    fn fusion_mode_defaults_to_rank_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert_eq!(RrfFusion::from_env(), RrfFusion::Rank);
        clear_env();
    }

    #[test]
    fn fusion_mode_reads_score_case_insensitively() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        for value in ["score", "SCORE", "Score", "  score  "] {
            crate::test_env::set_var(ENV_RRF_FUSION, value);
            assert_eq!(RrfFusion::from_env(), RrfFusion::Score, "{value:?}");
        }
        clear_env();
    }

    #[test]
    fn an_unrecognized_fusion_string_falls_back_to_rank_not_an_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(ENV_RRF_FUSION, "banana");
        assert_eq!(RrfFusion::from_env(), RrfFusion::Rank);
        clear_env();
    }

    #[test]
    fn choose_rrf_weights_composes_the_auto_preset_with_an_env_configured_base() {
        // With the base keyword weight doubled by env, a keyword-shaped
        // query's 1.5x preset multiplier should land at 3.0, not silently
        // discard the env override in favor of a fixed absolute number.
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(ENV_RRF_W_KEYWORD, "2.0");
        let w = choose_rrf_weights("id", RetrievalStrategy::Auto);
        assert_eq!(w.w_keyword, 3.0);
        clear_env();
    }

    #[test]
    fn choose_rrf_weights_never_resurrects_a_signal_the_base_zeroed_out() {
        // REMIND_ME_RRF_W_IDF=0 (the default) must stay 0 under every
        // strategy preset, keyword_favored included -- 0 * 1.5 == 0.
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let w = choose_rrf_weights("id", RetrievalStrategy::KeywordFavored);
        assert_eq!(w.w_idf, 0.0);
        clear_env();
    }

    fn mem(id: &str, vitality: f64) -> Memory {
        Memory {
            id: id.to_string(),
            content: String::new(),
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
            vitality,
            base_weight: 0.0,
            access_count: 0,
            accessed_at: String::new(),
            doc_id: None,
            chunk_index: None,
            remind_at: None,
            sensitive: false,
            // These ranking tests care only about vitality; the fields exist
            // so a `Memory` can round-trip to JSON (#198).
            memory_type: None,
            status: None,
            node_id: None,
            client: None,
            source_capture_id: None,
            deleted_at: None,
        }
    }

    fn rank_only(weights: RrfWeights) -> RrfConfig {
        RrfConfig {
            k: RRF_K_DEFAULT,
            weights,
            fusion: RrfFusion::Rank,
        }
    }

    fn find<'a>(ranked: &'a [MemorySearchResult], id: &str) -> &'a MemorySearchResult {
        ranked
            .iter()
            .find(|r| r.memory.id == id)
            .unwrap_or_else(|| panic!("{id} missing from ranked results"))
    }

    #[test]
    fn a_memory_present_in_both_lists_outranks_one_present_in_only_one() {
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let semantic = vec![mem("a", 0.0), mem("c", 0.0)];

        let ranked = rank_rrf(
            keyword,
            semantic,
            rank_only(RrfWeights::default()),
            &RrfSignals::default(),
        );

        assert_eq!(
            ranked[0].memory.id, "a",
            "top-ranked in both lists beats everything else"
        );
    }

    #[test]
    fn an_empty_semantic_list_means_semantic_never_ran_not_a_tie() {
        // Before #49, `semantic` was always empty because nothing produced
        // vectors -- vec_score must report None here, not a misleading
        // constant that looks like it carries information.
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];

        let ranked = rank_rrf(
            keyword,
            vec![],
            rank_only(RrfWeights::default()),
            &RrfSignals::default(),
        );

        assert!(ranked.iter().all(|r| r.vec_score.is_none()));
    }

    #[test]
    fn a_memory_found_by_semantic_search_only_still_gets_a_real_vec_score() {
        // Distinct from the "never ran" case: semantic DID run and ranked
        // this memory, even though keyword search never found it.
        let keyword = vec![mem("a", 0.0)];
        let semantic = vec![mem("b", 0.0)];

        let ranked = rank_rrf(
            keyword,
            semantic,
            rank_only(RrfWeights::default()),
            &RrfSignals::default(),
        );

        let b = find(&ranked, "b");
        assert!(b.vec_score.is_some());
        assert!(
            b.fts_score.is_some(),
            "fts_score is always Some, penalty-ranked or not"
        );
    }

    #[test]
    fn a_memory_absent_from_both_lists_is_impossible_the_union_only_contains_seen_ids() {
        let keyword = vec![mem("a", 0.0)];
        let semantic = vec![mem("b", 0.0)];

        let ranked = rank_rrf(
            keyword,
            semantic,
            rank_only(RrfWeights::default()),
            &RrfSignals::default(),
        );

        assert_eq!(
            ranked.len(),
            2,
            "the union is exactly [a, b] -- no phantom entries"
        );
    }

    #[test]
    fn a_duplicate_between_both_lists_appears_once_in_the_union() {
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let semantic = vec![mem("b", 0.0), mem("a", 0.0)];

        let ranked = rank_rrf(
            keyword,
            semantic,
            rank_only(RrfWeights::default()),
            &RrfSignals::default(),
        );

        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn semantic_favored_weights_can_flip_the_ordering_relative_to_keyword_favored() {
        // "a" ranks #1 by keyword and last by semantic; "b" is the reverse.
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let semantic = vec![mem("b", 0.0), mem("a", 0.0)];

        let keyword_favored = rank_rrf(
            keyword.clone(),
            semantic.clone(),
            rank_only(RrfWeights {
                w_keyword: 1.5,
                w_semantic: 0.1,
                w_recency: 0.0,
                w_vitality: 0.0,
                w_idf: 0.0,
            }),
            &RrfSignals::default(),
        );
        let semantic_favored = rank_rrf(
            keyword,
            semantic,
            rank_only(RrfWeights {
                w_keyword: 0.1,
                w_semantic: 1.5,
                w_recency: 0.0,
                w_vitality: 0.0,
                w_idf: 0.0,
            }),
            &RrfSignals::default(),
        );

        assert_eq!(keyword_favored[0].memory.id, "a");
        assert_eq!(semantic_favored[0].memory.id, "b");
    }

    #[test]
    fn a_vital_memory_outranks_a_dormant_one_all_else_equal() {
        // "All else equal" means isolating the vitality signal specifically
        // -- both memories share an (empty, tied) created_at, so leaving
        // w_recency on would let its stable-sort tie-break silently favor
        // whichever memory is listed first instead of testing vitality.
        let keyword = vec![mem("dormant", 0.0), mem("vital", 1.0)];

        let ranked = rank_rrf(
            keyword,
            vec![],
            rank_only(RrfWeights {
                w_keyword: 0.0,
                w_semantic: 0.0,
                w_recency: 0.0,
                w_vitality: 1.0,
                w_idf: 0.0,
            }),
            &RrfSignals::default(),
        );

        assert_eq!(ranked[0].memory.id, "vital");
    }

    #[test]
    fn rank_mode_and_score_mode_produce_different_orderings() {
        // "a" barely edges "b" on keyword rank (adjacent positions), but "b"
        // has a dramatically stronger raw bm25 match. Rank-mode fusion only
        // sees ordinal position and keeps "a" on top; score-mode fusion sees
        // the magnitude gap and should flip it to "b".
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let mut signals = RrfSignals::default();
        signals.keyword_bm25.insert("a".to_string(), -1.0);
        signals.keyword_bm25.insert("b".to_string(), -50.0);

        let weights = RrfWeights {
            w_keyword: 1.0,
            w_semantic: 0.0,
            w_recency: 0.0,
            w_vitality: 0.0,
            w_idf: 0.0,
        };

        let rank_mode = rank_rrf(
            keyword.clone(),
            vec![],
            RrfConfig {
                k: RRF_K_DEFAULT,
                weights,
                fusion: RrfFusion::Rank,
            },
            &signals,
        );
        let score_mode = rank_rrf(
            keyword,
            vec![],
            RrfConfig {
                k: RRF_K_DEFAULT,
                weights,
                fusion: RrfFusion::Score,
            },
            &signals,
        );

        assert_eq!(rank_mode[0].memory.id, "a");
        assert_eq!(score_mode[0].memory.id, "b");
    }

    #[test]
    fn score_mode_gives_a_missing_signal_the_worst_possible_normalized_value() {
        // "a" has no bm25 entry at all (a semantic-only hit); "b" does. In
        // score mode a missing signal contributes 0.0, the worst possible
        // normalized score, mirroring rank mode's penalty-rank treatment.
        let keyword = vec![mem("b", 0.0)];
        let semantic = vec![mem("a", 0.0), mem("b", 0.0)];
        let mut signals = RrfSignals::default();
        signals.keyword_bm25.insert("b".to_string(), -5.0);

        let ranked = rank_rrf(
            keyword,
            semantic,
            RrfConfig {
                k: RRF_K_DEFAULT,
                weights: RrfWeights {
                    w_keyword: 1.0,
                    w_semantic: 0.0,
                    w_recency: 0.0,
                    w_vitality: 0.0,
                    w_idf: 0.0,
                },
                fusion: RrfFusion::Score,
            },
            &signals,
        );

        assert_eq!(find(&ranked, "a").fts_score, Some(0.0));
        assert!(find(&ranked, "b").fts_score.unwrap() > 0.0);
    }

    #[test]
    fn score_mode_idf_reuses_the_keyword_signals_normalized_magnitude() {
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let mut signals = RrfSignals::default();
        signals.keyword_bm25.insert("a".to_string(), -1.0);
        signals.keyword_bm25.insert("b".to_string(), -10.0);

        let ranked = rank_rrf(
            keyword,
            vec![],
            RrfConfig {
                k: RRF_K_DEFAULT,
                weights: RrfWeights {
                    w_keyword: 0.0,
                    w_semantic: 0.0,
                    w_recency: 0.0,
                    w_vitality: 0.0,
                    w_idf: 1.0,
                },
                fusion: RrfFusion::Score,
            },
            &signals,
        );

        // Lower (more negative) bm25 is a better match, and normalization
        // inverts it -- "b" should score higher on idf than "a".
        assert!(find(&ranked, "b").idf_score.unwrap() > find(&ranked, "a").idf_score.unwrap());
    }

    #[test]
    fn an_idf_weight_of_zero_matches_the_reference_default_and_has_no_effect() {
        let keyword = vec![mem("a", 0.0), mem("b", 0.0)];
        let mut signals = RrfSignals::default();
        signals.keyword_bm25.insert("a".to_string(), -1.0);
        signals.keyword_bm25.insert("b".to_string(), -99.0);

        let ranked = rank_rrf(
            keyword,
            vec![],
            rank_only(RrfWeights {
                w_keyword: 1.0,
                w_semantic: 0.0,
                w_recency: 0.0,
                w_vitality: 0.0,
                w_idf: 0.0,
            }),
            &signals,
        );

        assert_eq!(find(&ranked, "a").idf_score, Some(0.0));
        assert_eq!(find(&ranked, "b").idf_score, Some(0.0));
    }
}
