//! A search window inferred from a date phrase in the query text (card #63).
//!
//! "deploy decisions last week" searches last week for "deploy decisions"
//! instead of matching the words "last" and "week". Off unless
//! [`FLAG_ENV`] is `on`, until the retrieval eval shows the lift. Applied only
//! at the search tool's boundary (`search_with_expansions`), never inside the
//! ranker, so saved searches, the brief, export and the daemon keep searching
//! their text as written. An explicit `since`/`until` always wins.

use crate::date_phrase::{self, Span};
use crate::models::MemorySearchInput;
use crate::time_window::TimeWindow;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// `on` turns inference on; anything else leaves it off.
pub const FLAG_ENV: &str = "REMIND_ME_DATE_PHRASES";

/// Whether the setting asks for inference.
pub fn enabled() -> bool {
    std::env::var(FLAG_ENV)
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case("on")
}

/// What inference applied, reported on the search response so a caller can
/// see why rows were filtered and repeat the search with an explicit window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferredWindow {
    /// The phrase as it appeared in the query.
    pub phrase: String,
    pub since: DateTime<Utc>,
    /// `None`: up to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
    /// The IANA zone whose calendar the phrase was read in.
    pub zone: String,
}

/// Words that carry no topic once a date phrase is gone. When nothing else is
/// left ("what happened last week"), the search lists the window instead of
/// matching these.
const FILLERS: &[&str] = &[
    "a",
    "about",
    "all",
    "an",
    "any",
    "anything",
    "are",
    "did",
    "do",
    "does",
    "done",
    "during",
    "everything",
    "for",
    "from",
    "happen",
    "happened",
    "has",
    "have",
    "i",
    "in",
    "is",
    "list",
    "me",
    "new",
    "of",
    "on",
    "over",
    "show",
    "so",
    "the",
    "there",
    "this",
    "was",
    "we",
    "were",
    "what",
    "what's",
    "whats",
    "you",
];

/// `input` narrowed to the window the first relative date phrase in its query
/// names, with the phrase taken out of the query, and what was applied.
///
/// `None` when the caller set `since` or `until`, or the query has no relative
/// phrase. `zone` is asked for only once a phrase is found; its error is
/// returned as is.
pub fn infer(
    input: &MemorySearchInput,
    now: DateTime<Utc>,
    zone: impl Fn() -> Result<Tz, String>,
) -> Result<Option<(MemorySearchInput, InferredWindow)>, String> {
    if !input.window.is_unbounded()
        || date_phrase::find_relative(&input.query, now.with_timezone(&Tz::UTC)).is_none()
    {
        return Ok(None);
    }
    let tz = zone()?;
    let Some((span, range)) = date_phrase::find_relative(&input.query, now.with_timezone(&tz))
    else {
        return Ok(None);
    };
    let query = &input.query;
    let narrowed = MemorySearchInput {
        query: without_phrase(query, range.clone()),
        window: TimeWindow::from_span(input.window.field(), span),
        ..input.clone()
    };
    Ok(Some((narrowed, applied(&query[range], span, tz))))
}

/// `query` without the phrase at `range` or the fillers that led into it
/// ("in the past week"); empty when only fillers would be left.
fn without_phrase(query: &str, range: std::ops::Range<usize>) -> String {
    let is_filler = |w: &&str| FILLERS.contains(&date_phrase::normalize_word(w).as_str());
    let mut before: Vec<&str> = query[..range.start].split_whitespace().collect();
    while before.last().is_some_and(is_filler) {
        before.pop();
    }
    let rest: Vec<&str> = before
        .into_iter()
        .chain(query[range.end..].split_whitespace())
        .collect();
    if rest.iter().all(is_filler) {
        return String::new();
    }
    rest.join(" ")
}

fn applied(phrase: &str, span: Span, tz: Tz) -> InferredWindow {
    InferredWindow {
        phrase: phrase.to_string(),
        since: span.start,
        until: span.end,
        zone: tz.name().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_window::{RawTimeWindow, TimeField};

    /// Wednesday 2026-10-28 09:00 UTC.
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-28T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn utc() -> Result<Tz, String> {
        Ok(Tz::UTC)
    }

    fn search(query: &str) -> MemorySearchInput {
        MemorySearchInput {
            query: query.into(),
            ..Default::default()
        }
    }

    fn infer_utc(query: &str) -> Option<(MemorySearchInput, InferredWindow)> {
        infer(&search(query), now(), utc).unwrap()
    }

    fn window(w: &TimeWindow) -> (Option<String>, Option<String>) {
        let raw = RawTimeWindow::from(*w);
        (raw.since, raw.until)
    }

    #[test]
    fn a_phrase_becomes_the_window_and_leaves_the_query() {
        let (narrowed, applied) = infer_utc("deploy decisions last week").unwrap();
        assert_eq!(narrowed.query, "deploy decisions");
        assert_eq!(
            window(&narrowed.window),
            (
                Some("2026-10-19T00:00:00+00:00".into()),
                Some("2026-10-26T00:00:00+00:00".into())
            )
        );
        assert_eq!(applied.phrase, "last week");
        assert_eq!(applied.zone, "UTC");
        assert_eq!(
            applied.until.map(|t| t.to_rfc3339()),
            window(&narrowed.window).1
        );
    }

    #[test]
    fn only_fillers_left_lists_the_window() {
        for query in [
            "what happened last week",
            "What did we do yesterday?",
            "anything new since monday",
            "yesterday",
        ] {
            let (narrowed, _) = infer_utc(query).unwrap_or_else(|| panic!("{query:?}"));
            assert_eq!(narrowed.query, "", "{query:?}");
        }
        let (narrowed, _) = infer_utc("what did we decide last week").unwrap();
        assert_eq!(narrowed.query, "what did we decide");
    }

    #[test]
    fn fillers_leading_into_the_phrase_go_with_it() {
        for (query, left) in [
            ("sticker campaign in the past week", "sticker campaign"),
            ("deploys over the last 3 days, rollback", "deploys rollback"),
            (
                "notes from yesterday about the lease",
                "notes about the lease",
            ),
        ] {
            let (narrowed, _) = infer_utc(query).unwrap_or_else(|| panic!("{query:?}"));
            assert_eq!(narrowed.query, left, "{query:?}");
        }
    }

    #[test]
    fn a_rolling_phrase_runs_up_to_now() {
        let (narrowed, applied) = infer_utc("app crashes past 3 days").unwrap();
        assert_eq!(narrowed.query, "app crashes");
        assert_eq!(applied.until, None);
        assert_eq!(
            window(&narrowed.window),
            (Some("2026-10-25T09:00:00+00:00".into()), None)
        );
    }

    #[test]
    fn an_explicit_window_or_no_phrase_changes_nothing() {
        let mut explicit = search("deploys last week");
        explicit.window = serde_json::from_value(serde_json::json!({ "since": "2d" })).unwrap();
        assert!(infer(&explicit, now(), utc).unwrap().is_none());
        for query in [
            "Q3 roadmap",
            "pricing recently",
            "in March",
            "deploys",
            "deploys since March 2025",
        ] {
            assert!(infer_utc(query).is_none(), "{query:?}");
        }
    }

    #[test]
    fn the_time_field_is_kept() {
        let mut input = search("edits yesterday");
        input.window =
            serde_json::from_value(serde_json::json!({ "time_field": "updated" })).unwrap();
        let (narrowed, _) = infer(&input, now(), utc).unwrap().unwrap();
        assert_eq!(narrowed.window.field(), TimeField::Updated);
    }

    #[test]
    fn the_zone_is_read_only_for_a_phrase_and_its_calendar_is_used() {
        let failing = || Err::<Tz, _>("bad zone".to_string());
        assert!(infer(&search("deploys"), now(), failing).unwrap().is_none());
        assert_eq!(
            infer(&search("deploys yesterday"), now(), failing).err(),
            Some("bad zone".to_string())
        );
        let sydney = || Ok(chrono_tz::Australia::Sydney);
        let (_, applied) = infer(&search("deploys yesterday"), now(), sydney)
            .unwrap()
            .unwrap();
        // 09:00 UTC is 20:00 AEDT on the 28th, so yesterday is the 27th there.
        assert_eq!(applied.since.to_rfc3339(), "2026-10-26T13:00:00+00:00");
        assert_eq!(applied.zone, "Australia/Sydney");
    }
}
