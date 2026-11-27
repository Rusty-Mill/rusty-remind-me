//! Recency buckets for status questions (card #568). Off unless
//! `REMIND_ME_TIME_BUCKETS=recency`.
//!
//! A search with no time window keeps at least one slot in the top `limit`
//! for the best hit of each non-empty bucket: the last 2 days, the 5 days
//! before, and older. Access boosting then cannot crowd the newest week out
//! of a status question ("latest on X"). It runs on the whole ranked list
//! after fusion and rerank, before `limit` and the token budget, so the same
//! query pages the same way. The spike also measured a `window` mode, which
//! spread a wide window across equal buckets; it lowered temporal MRR and was
//! dropped (`docs/research/568-time-buckets-spike.md`).
//!
//! The bucketing is pure: the clock and the timestamp are passed in.

use crate::models::{Memory, MemorySearchResult};
use crate::time_window::{parse_rfc3339, TimeField, TimeWindow};
use chrono::{DateTime, Duration, Utc};

/// The setting that turns recency buckets on: `recency`, or `off`.
pub const FLAG_ENV: &str = "REMIND_ME_TIME_BUCKETS";

/// Whether recency buckets are on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Rank order is left alone.
    #[default]
    Off,
    /// Reserve top slots for the newest buckets on an unwindowed search.
    Recency,
}

impl Mode {
    /// Read a setting's value. Unset, blank and anything unrecognised read
    /// as off, as `REMIND_ME_DATE_PHRASES` does, so it never turns on by
    /// accident.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "recency" => Self::Recency,
            _ => Self::Off,
        }
    }

    /// [`Self::parse`] of [`FLAG_ENV`].
    pub fn from_env() -> Self {
        Self::parse(&std::env::var(FLAG_ENV).unwrap_or_default())
    }
}

/// The instant a memory is bucketed by: its event time (a span's `to`)
/// under [`TimeField::Occurred`], else the field the window reads.
pub fn instant_of(m: &Memory, field: TimeField) -> Option<DateTime<Utc>> {
    match field {
        TimeField::Created => parse_rfc3339(&m.created_at),
        TimeField::Updated => parse_rfc3339(&m.updated_at),
        TimeField::Occurred => crate::kinds::OccurredAt::of(&m.metadata)
            .map(crate::kinds::OccurredAt::latest)
            .or_else(|| parse_rfc3339(&m.created_at)),
    }
}

/// Apply `mode` to a whole ranked list. Recency applies only to a search
/// with no window; anything else is returned unchanged.
pub fn apply(
    ranked: Vec<MemorySearchResult>,
    mode: Mode,
    window: &TimeWindow,
    limit: usize,
    now: DateTime<Utc>,
) -> Vec<MemorySearchResult> {
    if mode != Mode::Recency || !window.is_unbounded() {
        return ranked;
    }
    let field = window.field();
    reserve_recent(ranked, limit, now, |r| instant_of(&r.memory, field))
}

/// A half-open `[start, end)` span of time; `None` is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bucket {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
}

impl Bucket {
    fn holds(&self, at: DateTime<Utc>) -> bool {
        self.start.is_none_or(|s| at >= s) && self.end.is_none_or(|e| at < e)
    }
}

/// The recency buckets, newest first: the last 2 days, the 5 before, older.
fn recency_buckets(now: DateTime<Utc>) -> [Bucket; 3] {
    let two = now - Duration::days(2);
    let seven = now - Duration::days(7);
    [
        Bucket {
            start: Some(two),
            end: None,
        },
        Bucket {
            start: Some(seven),
            end: Some(two),
        },
        Bucket {
            start: None,
            end: Some(seven),
        },
    ]
}

/// The bucket each item falls in, by index; an item with no timestamp, or
/// outside every bucket, falls in the oldest.
fn assign<T>(
    items: &[T],
    buckets: &[Bucket],
    instant: &impl Fn(&T) -> Option<DateTime<Utc>>,
) -> Vec<usize> {
    let oldest = buckets.len().saturating_sub(1);
    items
        .iter()
        .map(|item| {
            instant(item)
                .and_then(|at| buckets.iter().position(|b| b.holds(at)))
                .unwrap_or(oldest)
        })
        .collect()
}

/// Make sure the best item of every non-empty recency bucket is in the
/// first `limit`, displacing the lowest-ranked item of a bucket that already
/// has more than one there. Everything keeps its rank order.
pub fn reserve_recent<T>(
    ranked: Vec<T>,
    limit: usize,
    now: DateTime<Utc>,
    instant: impl Fn(&T) -> Option<DateTime<Utc>>,
) -> Vec<T> {
    if limit == 0 || ranked.len() <= limit {
        return ranked;
    }
    let buckets = recency_buckets(now);
    let slots = assign(&ranked, &buckets, &instant);
    let mut chosen: Vec<bool> = (0..ranked.len()).map(|i| i < limit).collect();
    let mut held = [0usize; 3];
    for &slot in &slots[..limit] {
        held[slot] += 1;
    }
    for bucket in 0..buckets.len() {
        if held[bucket] > 0 {
            continue;
        }
        let Some(best) = (limit..ranked.len()).find(|&i| slots[i] == bucket) else {
            continue;
        };
        let Some(drop) = (0..ranked.len())
            .rev()
            .find(|&i| chosen[i] && held[slots[i]] > 1)
        else {
            continue;
        };
        held[slots[drop]] -= 1;
        chosen[drop] = false;
        held[bucket] += 1;
        chosen[best] = true;
    }
    let (mut top, mut rest) = (Vec::with_capacity(limit), Vec::new());
    for (item, keep) in ranked.into_iter().zip(chosen) {
        if keep {
            top.push(item);
        } else {
            rest.push(item);
        }
    }
    top.extend(rest);
    top
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Items are `(name, days ago)`; a negative age has no timestamp.
    fn at(item: &(&str, i64)) -> Option<DateTime<Utc>> {
        (item.1 >= 0).then(|| now() - Duration::days(item.1))
    }

    fn names(items: Vec<(&'static str, i64)>) -> Vec<&'static str> {
        items.into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn mode_parses_and_defaults_off() {
        assert_eq!(Mode::default(), Mode::Off);
        for raw in ["", "off", "on", "window", "both"] {
            assert_eq!(Mode::parse(raw), Mode::Off, "{raw}");
        }
        assert_eq!(Mode::parse(" Recency "), Mode::Recency);
    }

    #[test]
    fn recency_reserves_a_slot_for_the_newest_week() {
        // Heavily accessed old hits fill the top 3; the newest sit below.
        let ranked = vec![
            ("o1", 20),
            ("o2", 21),
            ("o3", 22),
            ("fresh", 1),
            ("week", 5),
        ];
        assert_eq!(
            names(reserve_recent(ranked, 3, now(), at)),
            ["o1", "fresh", "week", "o2", "o3"]
        );
    }

    #[test]
    fn recency_leaves_a_represented_top_alone() {
        let ranked = vec![("fresh", 1), ("week", 5), ("old", 20), ("old2", 30)];
        assert_eq!(
            names(reserve_recent(ranked.clone(), 3, now(), at)),
            names(ranked)
        );
    }

    #[test]
    fn recency_with_a_short_list_or_no_timestamps_is_unchanged() {
        let short = vec![("a", 20), ("b", 1)];
        assert_eq!(names(reserve_recent(short, 3, now(), at)), ["a", "b"]);
        let undated = vec![("a", -1), ("b", -1), ("c", -1), ("d", -1)];
        assert_eq!(
            names(reserve_recent(undated, 2, now(), at)),
            ["a", "b", "c", "d"]
        );
    }
}
