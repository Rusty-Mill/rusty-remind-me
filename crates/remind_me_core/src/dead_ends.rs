//! Dead ends: approaches that were tried and failed, surfaced as warnings
//! before someone tries them again (card #1484, pack D115-D117, D123).
//!
//! A dead end is a memory of kind `dead_end` with `metadata.tried` and
//! `metadata.why_failed` (and optionally `cost`). This module finds the dead
//! ends a request should be warned about. Search, the brief and the prompt
//! hook show them in a `warnings` section outside the ranking.
//!
//! # Matching (D116)
//!
//! Lexical only, so the answer is the same with or without an embedder.
//! The dead end's words (from `tried` and its content, lowercased, without
//! stopwords and words under 3 characters) are compared with the query's.
//! It matches when at least half of its words appear in the query and at
//! least two do (one, for a dead end of one word). The overlap is measured
//! against the dead end, not both sides, because a prompt is usually much
//! longer than the thing it repeats.
//!
//! A candidate must be live, unsuperseded, unexpired and not a dialog;
//! sensitive only when the caller asked for sensitive rows. Dormancy is
//! ignored: a dead end does not stop being dead by not being read. With a
//! project given, its `project` must equal it or be unset; with none, every
//! project is admitted.

use crate::db::memories::Memories;
use crate::db::{Result, Store};
use crate::kinds::MemoryKind;
use crate::models::Memory;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Most warnings a search returns.
pub const SEARCH_LIMIT: usize = 3;
/// Most warnings a project or session brief shows.
pub const BRIEF_LIMIT: usize = 3;
/// Most warnings the per-prompt hook shows.
pub const PROMPT_LIMIT: usize = 2;
/// Share of a dead end's words the query must contain.
pub const MIN_SCORE: f64 = 0.5;
/// Fewest shared words, for a dead end of more than one word.
pub const MIN_SHARED: usize = 2;
/// Shortest word that counts.
const MIN_TOKEN_CHARS: usize = 3;
/// Characters of `tried` and `why_failed` a compact rendering keeps.
pub const COMPACT_CHARS: usize = 200;

/// Words too common to say what a dead end was about.
const STOPWORDS: &[&str] = &[
    "about", "after", "again", "all", "also", "and", "any", "are", "because", "been", "before",
    "but", "can", "could", "did", "does", "doing", "done", "for", "from", "get", "got", "had",
    "has", "have", "how", "into", "its", "just", "like", "make", "more", "most", "not", "now",
    "only", "other", "our", "out", "over", "should", "some", "than", "that", "the", "their",
    "them", "then", "there", "these", "they", "this", "those", "through", "too", "try", "tried",
    "trying", "use", "used", "using", "very", "was", "way", "were", "what", "when", "where",
    "which", "while", "who", "why", "will", "with", "would", "you", "your",
];

/// One warning: a dead end, as search and the brief show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    pub id: String,
    pub tried: String,
    pub why_failed: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub created_at: String,
    /// The share of the dead end's words the query held; `1.0` when no
    /// query was matched (a brief's scope listing).
    pub score: f64,
}

impl Warning {
    fn of(memory: &Memory, score: f64) -> Self {
        let text = |key: &str| {
            memory
                .metadata
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Self {
            id: memory.id.clone(),
            tried: text("tried"),
            why_failed: text("why_failed"),
            cost: memory
                .metadata
                .get("cost")
                .filter(|v| !v.is_null())
                .cloned(),
            project: memory.project.clone(),
            created_at: memory.created_at.clone(),
            score,
        }
    }

    /// This warning with `tried` and `why_failed` cut to `chars` characters.
    pub fn cut(&self, chars: usize) -> Self {
        Self {
            tried: cut(&self.tried, chars),
            why_failed: cut(&self.why_failed, chars),
            ..self.clone()
        }
    }

    /// One Markdown line.
    pub fn markdown(&self) -> String {
        let cost = match &self.cost {
            Some(Value::Number(n)) => format!(" (cost: {n} min)"),
            Some(Value::String(s)) => format!(" (cost: {s})"),
            _ => String::new(),
        };
        format!(
            "- ⚠ `{}` tried: {} — failed: {}{cost}",
            self.id, self.tried, self.why_failed
        )
    }
}

fn cut(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        None => text.to_string(),
        Some((at, _)) => format!("{}…", &text[..at]),
    }
}

/// What to find warnings for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    /// Matched lexically when given; `None` lists every dead end in scope.
    pub query: Option<&'a str>,
    /// A project scope; `None` admits every project.
    pub project: Option<&'a str>,
    pub include_sensitive: bool,
}

/// The warnings found, best first, and how many more matched than `limit`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    pub warnings: Vec<Warning>,
    pub omitted: usize,
}

/// Every dead end `request` should be warned about, at most `limit`, best
/// first (higher score, then newer). `admit` is a last filter the caller
/// applies (a brief's scope and mutes).
pub fn find(
    store: &Store<'_>,
    request: &Request<'_>,
    limit: usize,
    now: DateTime<Utc>,
    admit: &dyn Fn(&Memory) -> bool,
) -> Result<Found> {
    let candidates = Memories::new(store).live_of_type(MemoryKind::DeadEnd.as_str())?;
    Ok(rank(&candidates, request, limit, now, admit))
}

/// [`find`] over `candidates` already read. Pure.
pub fn rank(
    candidates: &[Memory],
    request: &Request<'_>,
    limit: usize,
    now: DateTime<Utc>,
    admit: &dyn Fn(&Memory) -> bool,
) -> Found {
    let query = request.query.map(tokens);
    let mut matched: Vec<(f64, &Memory)> = candidates
        .iter()
        .filter(|m| showable(m, request, now) && admit(m))
        .filter_map(|m| match &query {
            None => Some((1.0, m)),
            Some(query) => score(m, query).map(|s| (s, m)),
        })
        .collect();
    matched.sort_by(|(sa, a), (sb, b)| {
        sb.total_cmp(sa)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    let omitted = matched.len().saturating_sub(limit);
    Found {
        warnings: matched
            .into_iter()
            .take(limit)
            .map(|(s, m)| Warning::of(m, s))
            .collect(),
        omitted,
    }
}

/// Live, unsuperseded, unexpired, not a dialog, sensitive only when asked,
/// in the project scope.
fn showable(memory: &Memory, request: &Request<'_>, now: DateTime<Utc>) -> bool {
    memory.memory_type.as_deref() == Some(MemoryKind::DeadEnd.as_str())
        && memory.deleted_at.is_none()
        && memory.superseded_by.is_none()
        && memory.category != crate::models::DIALOG_CATEGORY
        && (request.include_sensitive || !memory.sensitive)
        && !crate::retrieval::is_expired(now, memory.valid_until.as_deref())
        && in_project(request.project, memory)
}

/// Whether `memory` is in the `project` scope: its own project is that one
/// (case and spaces aside) or unset. `None` admits every project.
pub fn in_project(project: Option<&str>, memory: &Memory) -> bool {
    match (project, memory.project.as_deref()) {
        (Some(want), Some(have)) => want.trim().eq_ignore_ascii_case(have.trim()),
        _ => true,
    }
}

/// The share of `memory`'s words in `query`, when it clears the bar.
fn score(memory: &Memory, query: &BTreeSet<String>) -> Option<f64> {
    let tried = memory
        .metadata
        .get("tried")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let own = tokens(&format!("{tried} {}", memory.content));
    if own.is_empty() {
        return None;
    }
    let shared = own.intersection(query).count();
    let needed = MIN_SHARED.min(own.len());
    let score = shared as f64 / own.len() as f64;
    (shared >= needed && score >= MIN_SCORE).then_some(score)
}

/// Lowercased words of at least [`MIN_TOKEN_CHARS`], stopwords removed.
fn tokens(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= MIN_TOKEN_CHARS && !STOPWORDS.contains(t))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: &str = "2026-11-27T12:00:00Z";

    fn dead_end(id: &str, tried: &str, project: Option<&str>) -> Memory {
        let mut row = crate::db::memories::NewMemory::new(id, tried, "2026-11-20T00:00:00Z");
        row.memory_type = "dead_end".into();
        row.metadata = json!({ "tried": tried, "why_failed": "it broke", "cost": 30 });
        row.project = project.map(str::to_string);
        crate::db::engine::memories::MemoryRow::from_new(&row).to_memory()
    }

    fn now() -> DateTime<Utc> {
        NOW.parse().unwrap()
    }

    fn found(candidates: &[Memory], request: Request<'_>, limit: usize) -> Vec<String> {
        rank(candidates, &request, limit, now(), &|_| true)
            .warnings
            .into_iter()
            .map(|w| w.id)
            .collect()
    }

    fn asking(query: &str) -> Request<'_> {
        Request {
            query: Some(query),
            ..Request::default()
        }
    }

    #[test]
    fn a_prompt_repeating_the_approach_matches() {
        let rows = [dead_end(
            "d1",
            "rebuild the musl target with jemalloc",
            None,
        )];
        let ids = found(
            &rows,
            asking("let's rebuild musl with jemalloc for the release"),
            3,
        );
        assert_eq!(ids, ["d1"]);
    }

    #[test]
    fn an_unrelated_prompt_does_not_match() {
        let rows = [dead_end(
            "d1",
            "rebuild the musl target with jemalloc",
            None,
        )];
        assert!(found(&rows, asking("write the release notes for tuesday"), 3).is_empty());
        // One shared word of four is under the bar.
        assert!(found(&rows, asking("jemalloc profiling numbers"), 3).is_empty());
    }

    #[test]
    fn stopwords_and_short_words_do_not_count() {
        let tokens = tokens("We tried to use the DB on it");
        assert!(tokens.is_empty(), "{tokens:?}");
    }

    #[test]
    fn sensitive_ones_only_when_asked() {
        let mut row = dead_end("d1", "rebuild musl jemalloc", None);
        row.sensitive = true;
        let rows = [row];
        assert!(found(&rows, asking("rebuild musl jemalloc"), 3).is_empty());
        let asked = Request {
            include_sensitive: true,
            ..asking("rebuild musl jemalloc")
        };
        assert_eq!(found(&rows, asked, 3), ["d1"]);
    }

    #[test]
    fn another_projects_dead_end_is_left_out_and_a_projectless_one_kept() {
        let rows = [
            dead_end("ours", "rebuild musl jemalloc", Some("alpha")),
            dead_end("theirs", "rebuild musl jemalloc", Some("beta")),
            dead_end("everywhere", "rebuild musl jemalloc", None),
        ];
        let scoped = Request {
            project: Some("Alpha"),
            ..asking("rebuild musl jemalloc")
        };
        let mut ids = found(&rows, scoped, 5);
        ids.sort();
        assert_eq!(ids, ["everywhere", "ours"]);
        assert_eq!(found(&rows, asking("rebuild musl jemalloc"), 5).len(), 3);
    }

    #[test]
    fn superseded_expired_and_other_kinds_are_left_out() {
        let mut superseded = dead_end("s", "rebuild musl jemalloc", None);
        superseded.superseded_by = Some("x".into());
        let mut expired = dead_end("e", "rebuild musl jemalloc", None);
        expired.valid_until = Some("2026-11-01T00:00:00Z".into());
        let mut learning = dead_end("l", "rebuild musl jemalloc", None);
        learning.memory_type = Some("learning".into());
        let rows = [superseded, expired, learning];
        assert!(found(&rows, asking("rebuild musl jemalloc"), 3).is_empty());
    }

    #[test]
    fn the_cap_holds_and_counts_the_rest() {
        let rows: Vec<Memory> = (0..5)
            .map(|i| dead_end(&format!("d{i}"), "rebuild musl jemalloc", None))
            .collect();
        let got = rank(&rows, &asking("rebuild musl jemalloc"), 2, now(), &|_| true);
        assert_eq!(got.warnings.len(), 2);
        assert_eq!(got.omitted, 3);
    }

    #[test]
    fn a_better_match_ranks_first() {
        let rows = [
            dead_end("half", "rebuild musl jemalloc allocator tuning", None),
            dead_end("all", "rebuild musl jemalloc", None),
        ];
        assert_eq!(
            found(&rows, asking("rebuild musl jemalloc"), 3),
            ["all", "half"]
        );
    }

    #[test]
    fn without_a_query_every_dead_end_in_scope_is_listed_newest_first() {
        let mut older = dead_end("old", "a", None);
        older.created_at = "2026-01-01T00:00:00Z".into();
        let rows = [older, dead_end("new", "b", None)];
        assert_eq!(found(&rows, Request::default(), 3), ["new", "old"]);
    }

    #[test]
    fn the_admit_filter_applies() {
        let rows = [dead_end("d1", "rebuild musl jemalloc", None)];
        let got = rank(&rows, &asking("rebuild musl jemalloc"), 3, now(), &|_| {
            false
        });
        assert!(got.warnings.is_empty());
    }

    #[test]
    fn a_warning_carries_the_fields_and_renders_one_line() {
        let w = Warning::of(&dead_end("d1", "rebuild musl", None), 1.0);
        assert_eq!(w.tried, "rebuild musl");
        assert_eq!(w.why_failed, "it broke");
        assert_eq!(w.cost, Some(json!(30)));
        assert!(w.markdown().contains("cost: 30 min"), "{}", w.markdown());
        let long = Warning {
            tried: "x".repeat(300),
            ..w
        };
        assert_eq!(
            long.cut(COMPACT_CHARS).tried.chars().count(),
            COMPACT_CHARS + 1
        );
    }
}
