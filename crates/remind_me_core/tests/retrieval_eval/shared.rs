//! The #59 retrieval eval's fixture, golden set, ranker call and scores,
//! shared by `retrieval_eval_test.rs` and R3's `store_bench relevance`
//! (D58), so the two cannot drift. Each user takes a subset, hence the
//! `dead_code` allowance. Test support: a malformed fixture panics.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::io::Write;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use remind_me_core::context::ScopeFilter;
use remind_me_core::date_inference;
use remind_me_core::date_phrase;
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::{queries, Store};
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::entity::{link_memory_entity, upsert_entity};
use remind_me_core::retrieval::Deadline;
use remind_me_core::time_buckets::Mode;
use remind_me_core::topic::{self, Parent, TopicName};
use remind_me_core::vectors::embed_and_store;
use remind_me_core::{Database, EntityInput, MemorySearchInput, RetrievalStrategy, SearchFilters};
use serde::{Deserialize, Serialize};

/// How far a score may fall below its baseline before the test fails. Two
/// points is about one or two queries moving on a set this size.
pub const TOLERANCE: f64 = 0.02;
/// Results per query: recall@10 and the `must_not` check read this deep.
pub const DEPTH: usize = 10;
pub const STRATEGIES: [(&str, RetrievalStrategy); 4] = [
    ("auto", RetrievalStrategy::Auto),
    ("balanced", RetrievalStrategy::Balanced),
    ("keyword_favored", RetrievalStrategy::KeywordFavored),
    ("semantic_favored", RetrievalStrategy::SemanticFavored),
];

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// One stored memory. `age_days`, or the calendar window `during` names, is
/// resolved against the clock at load time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureMemory {
    pub id: String,
    pub content: String,
    pub category: String,
    pub memory_type: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub project: Option<String>,
    #[serde(default)]
    pub age_days: i64,
    /// A date phrase (`last week`, `this month`): the row is placed mid-way
    /// through that window, up to now. Not with `age_days`.
    pub during: Option<String>,
    /// Days since it was last read; default today. Old enough and the row is
    /// dormant, which search must hide from every stage.
    #[serde(default)]
    pub accessed_days: i64,
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub entities: Vec<String>,
    /// Topic paths the row belongs to, `Parent/Child`; each topic on the
    /// path is created under the one before it.
    #[serde(default)]
    pub topics: Vec<String>,
    /// When the row's event happened, stored as `metadata.occurred_at`
    /// (card #62); unset, it happened when it was recorded.
    pub occurred: Option<FixtureOccurred>,
}

/// A fixture row's `occurred_at`, resolved against the clock at load time.
/// Exactly one field is set.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureOccurred {
    /// An instant this many days back.
    pub days: Option<i64>,
    /// An instant mid-way through the window this date phrase names, as
    /// [`FixtureMemory::during`] places `created_at`.
    pub during: Option<String>,
    /// A `{from, to}` span, `[from, to]` days back, `from > to`.
    pub span_days: Option<[i64; 2]>,
}

/// One golden query: what it should find, and what it must never surface.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenQuery {
    pub query: String,
    pub relevant: Vec<String>,
    pub kind: String,
    #[serde(default)]
    pub must_not: Vec<String>,
    pub project: Option<String>,
    pub entity: Option<String>,
    /// A `topic` search filter (the topic and every topic below it).
    pub topic: Option<String>,
}

/// The fixture and the engine's blessed scores, compiled in relative to
/// this file, so any crate that includes it reads the same files.
pub const MEMORIES: &str = include_str!("../fixtures/retrieval_eval/memories.jsonl");
pub const QUERIES: &str = include_str!("../fixtures/retrieval_eval/queries.jsonl");
pub const BASELINE: &str = include_str!("../fixtures/retrieval_eval/baseline.json");

pub fn read_jsonl<T: for<'de> Deserialize<'de>>(name: &str) -> Vec<T> {
    let text = match name {
        "memories.jsonl" => MEMORIES,
        "queries.jsonl" => QUERIES,
        other => panic!("no fixture file {other}"),
    };
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(n, line)| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{name}:{}: {e}", n + 1))
        })
        .collect()
}

/// Refuse a golden set that cannot mean what it says: an empty or unknown
/// `relevant`, an unknown `must_not`, or an id that is both.
pub fn validate(memories: &[FixtureMemory], golden: &[GoldenQuery]) -> Result<(), String> {
    let known: HashSet<&str> = memories.iter().map(|m| m.id.as_str()).collect();
    if known.len() != memories.len() {
        return Err("duplicate memory id in memories.jsonl".into());
    }
    for m in memories {
        if m.during.is_some() && m.age_days != 0 {
            return Err(format!("{}: both during and age_days", m.id));
        }
        if let Some(o) = &m.occurred {
            let set = [o.days.is_some(), o.during.is_some(), o.span_days.is_some()];
            if set.iter().filter(|s| **s).count() != 1 {
                return Err(format!("{}: occurred needs exactly one field", m.id));
            }
            if o.span_days.is_some_and(|[from, to]| from <= to || to < 0) {
                return Err(format!(
                    "{}: occurred span_days must run back to front",
                    m.id
                ));
            }
        }
        if let Some(next) = &m.superseded_by {
            if !known.contains(next.as_str()) {
                return Err(format!(
                    "{}: superseded_by {next} is not in the fixture",
                    m.id
                ));
            }
        }
    }
    for g in golden {
        if g.relevant.is_empty() {
            return Err(format!("{:?}: no relevant ids", g.query));
        }
        for id in g.relevant.iter().chain(&g.must_not) {
            if !known.contains(id.as_str()) {
                return Err(format!("{:?}: unknown id {id}", g.query));
            }
        }
        if let Some(both) = g.must_not.iter().find(|id| g.relevant.contains(id)) {
            return Err(format!(
                "{:?}: {both} is both relevant and must_not",
                g.query
            ));
        }
    }
    Ok(())
}

/// Insert the fixture into `store`, link its entities and, given an
/// embedder, write its vectors.
pub fn load(store: &Store<'_>, memories: &[FixtureMemory], embedder: Option<&dyn Embedder>) {
    let now = Utc::now();
    let repo = Memories::new(store);
    for m in memories {
        let at = created_at(m, now).to_rfc3339();
        let metadata = match &m.occurred {
            Some(o) => serde_json::json!({ "occurred_at": occurred_at(m, o, now) }),
            None => serde_json::json!({}),
        };
        repo.insert(&NewMemory {
            metadata,
            category: m.category.clone(),
            memory_type: m.memory_type.clone(),
            tags: m.tags.clone(),
            project: m.project.clone(),
            superseded_by: m.superseded_by.clone(),
            sensitive: m.sensitive,
            accessed_at: Some((now - Duration::days(m.accessed_days)).to_rfc3339()),
            ..NewMemory::new(&m.id, &m.content, &at)
        })
        .unwrap();
        for name in &m.entities {
            let entity = upsert_entity(
                store,
                &EntityInput {
                    name: name.clone(),
                    kind: None,
                    aliases: Vec::new(),
                },
            )
            .unwrap();
            link_memory_entity(store, &m.id, &entity.id).unwrap();
        }
        for path in &m.topics {
            let leaf = ensure_topic_path(store, path, &m.id);
            topic::assign(store, &m.id, &leaf).unwrap();
        }
    }
    let Some(embedder) = embedder else { return };
    for m in memories {
        embed_and_store(store, embedder, &m.id, &m.content).unwrap();
    }
}

/// Create every topic on `path` (`A/B/C`) under the one before it, citing
/// `evidence` (a fixture row), and return the last. Idempotent.
pub fn ensure_topic_path(store: &Store<'_>, path: &str, evidence: &str) -> TopicName {
    let mut parent = Parent::Root;
    let mut last = None;
    for label in path.split('/') {
        let name = TopicName::parse(label).unwrap();
        let exists = topic::tree(store).unwrap().get(&name).is_some();
        if !exists {
            topic::set(
                store,
                &topic::SetInput {
                    topic: name.clone(),
                    parent: Some(parent),
                    status: Some(topic::Status::Active),
                    evidence: vec![evidence.to_string()],
                    because: None,
                    confirmed_by: "fixture".into(),
                },
            )
            .unwrap();
        }
        parent = Parent::Topic(name.clone());
        last = Some(name);
    }
    last.unwrap_or_else(|| panic!("empty topic path {path:?}"))
}

/// `age_days` back, or mid-way through the window `during` names (in UTC,
/// the zone the `+dates` modes infer in).
pub fn created_at(m: &FixtureMemory, now: DateTime<Utc>) -> DateTime<Utc> {
    let Some(phrase) = &m.during else {
        return now - Duration::days(m.age_days);
    };
    let span = date_phrase::parse(phrase, now.with_timezone(&Tz::UTC))
        .unwrap_or_else(|| panic!("{}: during {phrase:?} is not a date phrase", m.id));
    let end = span.end.map_or(now, |e| e.min(now));
    span.start + (end - span.start) / 2
}

/// `o` as the `metadata.occurred_at` value it stands for, at `now`.
pub fn occurred_at(
    m: &FixtureMemory,
    o: &FixtureOccurred,
    now: DateTime<Utc>,
) -> serde_json::Value {
    let back = |days: i64| (now - Duration::days(days)).to_rfc3339();
    if let Some(days) = o.days {
        return serde_json::json!(back(days));
    }
    if let Some([from, to]) = o.span_days {
        return serde_json::json!({ "from": back(from), "to": back(to) });
    }
    let phrase = o.during.as_deref().unwrap_or_default();
    let span = date_phrase::parse(phrase, now.with_timezone(&Tz::UTC))
        .unwrap_or_else(|| panic!("{}: occurred during {phrase:?} is not a date phrase", m.id));
    let end = span.end.map_or(now, |e| e.min(now));
    serde_json::json!((span.start + (end - span.start) / 2).to_rfc3339())
}

pub fn ranked(
    store: &Store<'_>,
    g: &GoldenQuery,
    strategy: RetrievalStrategy,
    embedder: Option<&dyn Embedder>,
    dates: bool,
) -> Vec<String> {
    ranked_outcome(store, g, strategy, embedder, dates).0
}

/// [`ranked`], and whether the search degraded (a stage skipped or
/// failed, so its ranking is a fallback's, not the full search's).
pub fn ranked_outcome(
    store: &Store<'_>,
    g: &GoldenQuery,
    strategy: RetrievalStrategy,
    embedder: Option<&dyn Embedder>,
    dates: bool,
) -> (Vec<String>, bool) {
    ranked_outcome_with(store, g, strategy, embedder, dates, Mode::Off)
}

/// [`ranked_outcome`] with card #568's recency buckets set as given.
pub fn ranked_outcome_with(
    store: &Store<'_>,
    g: &GoldenQuery,
    strategy: RetrievalStrategy,
    embedder: Option<&dyn Embedder>,
    dates: bool,
    time_buckets: Mode,
) -> (Vec<String>, bool) {
    let input = MemorySearchInput {
        query: g.query.clone(),
        limit: DEPTH,
        token_budget: 0,
        strategy,
        scope: ScopeFilter {
            project: g.project.clone(),
            ..Default::default()
        },
        filters: SearchFilters {
            entity: g.entity.clone(),
            topic: g.topic.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    let input = if dates {
        date_inference::infer(&input, Utc::now(), || Ok(Tz::UTC))
            .unwrap()
            .map_or(input, |(narrowed, _)| narrowed)
    } else {
        input
    };
    let outcome = queries::search_memories_deadlined_with(
        store,
        &input,
        embedder,
        Deadline::unbounded(),
        time_buckets,
    )
    .unwrap();
    let degraded = outcome.timing.degraded();
    let ids = outcome.results.into_iter().map(|r| r.memory.id).collect();
    (ids, degraded)
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Share of `relevant` found in the first `k` of `ranking`.
pub fn recall_at(ranking: &[String], relevant: &[String], k: usize) -> f64 {
    let top: HashSet<&String> = ranking.iter().take(k).collect();
    let found = relevant.iter().filter(|id| top.contains(id)).count();
    found as f64 / relevant.len() as f64
}

/// 1 / rank of the first relevant hit within [`DEPTH`], or 0.
pub fn reciprocal_rank(ranking: &[String], relevant: &[String]) -> f64 {
    ranking
        .iter()
        .take(DEPTH)
        .position(|id| relevant.contains(id))
        .map_or(0.0, |i| 1.0 / (i + 1) as f64)
}

/// Mean scores over a set of queries, rounded so the baseline diffs cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scores {
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub mrr: f64,
}

impl Scores {
    pub fn mean(rows: &[(f64, f64, f64)]) -> Self {
        let n = rows.len().max(1) as f64;
        let sum = |f: fn(&(f64, f64, f64)) -> f64| round4(rows.iter().map(f).sum::<f64>() / n);
        Self {
            recall_at_5: sum(|r| r.0),
            recall_at_10: sum(|r| r.1),
            mrr: sum(|r| r.2),
        }
    }

    pub fn named(&self) -> [(&'static str, f64); 3] {
        [
            ("recall@5", self.recall_at_5),
            ("recall@10", self.recall_at_10),
            ("mrr", self.mrr),
        ]
    }
}

pub fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// Every metric of `now` more than `tolerance` below `base`, by name.
pub fn regressions(base: &Scores, now: &Scores, tolerance: f64) -> Vec<String> {
    base.named()
        .iter()
        .zip(now.named())
        .filter(|((_, b), (_, n))| *n < b - tolerance - 1e-9)
        .map(|((name, b), (_, n))| format!("{name} {b:.4} -> {n:.4}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Embedders
// ---------------------------------------------------------------------------

/// Feature hashing of lowercased words and character trigrams into 256
/// dimensions. Deterministic and offline: it tests that the vector stage and
/// fusion work, not that anything understands meaning.
pub struct HashedEmbedder;

pub const HASH_DIM: usize = 256;

impl HashedEmbedder {
    pub fn vector(text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let mut v = vec![0f32; HASH_DIM];
        for word in lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            v[fnv1a(word.as_bytes()) % HASH_DIM] += 1.0;
            let padded: Vec<char> = format!(" {word} ").chars().collect();
            for gram in padded.windows(3) {
                let gram: String = gram.iter().collect();
                v[fnv1a(gram.as_bytes()) % HASH_DIM] += 0.5;
            }
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

pub fn fnv1a(bytes: &[u8]) -> usize {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3)
    }) as usize
}

impl Embedder for HashedEmbedder {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| Self::vector(t)).collect())
    }
    fn dim(&self) -> usize {
        HASH_DIM
    }
    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "eval-hashed".into(),
            model: "fnv1a-words-trigrams".into(),
            dim: HASH_DIM,
        }
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// Scores per `mode/strategy`, plus every hard failure seen: a `must_not`
/// hit, or an entity-filtered query that found nothing it should.
#[derive(Default)]
pub struct Report {
    pub scores: BTreeMap<String, Scores>,
    /// `+dates` runs' scores per `mode/strategy`: printed, never gated.
    pub measured: BTreeMap<String, Scores>,
    /// `mode/strategy/kind`, for the `+dates` lift lines; not gated.
    pub kinds: BTreeMap<String, Scores>,
    pub violations: Vec<String>,
    /// `mode/strategy` of each search that degraded, once per search.
    pub degraded: Vec<String>,
}

/// Score every golden query under each strategy in `mode`, on `db` (a
/// fresh, empty store) loaded with the fixture, into `report`; the per-kind
/// lines and misses go to `out`.
pub fn evaluate_in(
    mode: &str,
    embedder: Option<&dyn Embedder>,
    db: &Database,
    out: &mut dyn Write,
    report: &mut Report,
) {
    let dates = mode.ends_with("+dates");
    let memories: Vec<FixtureMemory> = read_jsonl("memories.jsonl");
    let golden: Vec<GoldenQuery> = read_jsonl("queries.jsonl");
    validate(&memories, &golden).unwrap();

    // Search here does not write (no access or co-retrieval bookkeeping), so
    // one store serves every strategy and query order cannot matter.
    let store = db.store();
    load(&store, &memories, embedder);

    for (name, strategy) in STRATEGIES {
        let mut all = Vec::new();
        let mut by_kind: BTreeMap<&str, Vec<(f64, f64, f64)>> = BTreeMap::new();
        for g in &golden {
            let (ranking, degraded) = ranked_outcome(&store, g, strategy, embedder, dates);
            if degraded {
                report.degraded.push(format!("{mode}/{name}"));
            }
            for leak in g.must_not.iter().filter(|id| ranking.contains(id)) {
                report
                    .violations
                    .push(format!("{mode}/{name}: {:?} surfaced {leak}", g.query));
            }
            let row = (
                recall_at(&ranking, &g.relevant, 5),
                recall_at(&ranking, &g.relevant, DEPTH),
                reciprocal_rank(&ranking, &g.relevant),
            );
            if g.entity.is_some() && row.1 == 0.0 {
                report.violations.push(format!(
                    "{mode}/{name}: {:?} with its entity filter found nothing relevant",
                    g.query
                ));
            }
            if g.topic.is_some() && row.1 == 0.0 {
                report.violations.push(format!(
                    "{mode}/{name}: {:?} with its topic filter found nothing relevant",
                    g.query
                ));
            }
            // Misses under the default router, so a drop names its queries.
            if strategy == RetrievalStrategy::Auto && row.1 < 1.0 {
                let _ = writeln!(
                    out,
                    "  miss [{}] {:?}: got {:?}",
                    g.kind,
                    g.query,
                    &ranking[..ranking.len().min(5)]
                );
            }
            all.push(row);
            by_kind.entry(g.kind.as_str()).or_default().push(row);
        }
        let key = format!("{mode}/{name}");
        let total = Scores::mean(&all);
        let _ = writeln!(out, "{key:32} {}", fmt(&total));
        for (kind, rows) in &by_kind {
            let scores = Scores::mean(rows);
            let _ = writeln!(out, "  {kind:30} {} (n={})", fmt(&scores), rows.len());
            report.kinds.insert(format!("{key}/{kind}"), scores);
        }
        if dates {
            report.measured.insert(key, total);
        } else {
            report.scores.insert(key, total);
        }
    }
}

/// Each kind's change under `auto` when date phrases are inferred, as
/// `(plain key, recall@5 lift, MRR lift)`: the measurement behind turning
/// `REMIND_ME_DATE_PHRASES` on by default.
pub fn date_lift(report: &Report) -> Vec<(String, f64, f64)> {
    report
        .kinds
        .iter()
        .filter(|(k, _)| k.contains("+dates/auto/"))
        .filter_map(|(key, dated)| {
            let plain_key = key.replace("+dates", "");
            let plain = report.kinds.get(&plain_key)?;
            Some((
                plain_key,
                dated.recall_at_5 - plain.recall_at_5,
                dated.mrr - plain.mrr,
            ))
        })
        .collect()
}

pub fn fmt(s: &Scores) -> String {
    format!(
        "r@5 {:.3}  r@10 {:.3}  mrr {:.3}",
        s.recall_at_5, s.recall_at_10, s.mrr
    )
}
