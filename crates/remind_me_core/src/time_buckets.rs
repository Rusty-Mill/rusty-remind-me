//! Spreading ranked hits across time buckets: the card #568 spike (pack
//! D102, keep rule D104). Off unless `REMIND_ME_TIME_BUCKETS` names a mode.
//!
//! - `window`: a search window wider than [`MIN_WINDOW_DAYS`] is cut into
//!   equal buckets, `clamp(days / 7, 3, 6)` of them, and the ranked list is
//!   interleaved round-robin, newest bucket first, score order inside each.
//!   A wide window ("last month") then cannot come back clustered on one day.
//! - `recency`: a search with no window keeps at least one slot in the top
//!   `limit` for the best hit of each non-empty bucket of the last 2 days,
//!   the last 7 days and older, so access boosting cannot crowd the newest
//!   week out of a status question ("latest on X").
//!
//! Both run on the whole ranked list after fusion and rerank, before
//! `limit` and the token budget, so the same query pages the same way. The
//! functions here are pure: the clock and the timestamp are passed in.

use crate::models::{Memory, MemorySearchResult};
use crate::time_window::{parse_rfc3339, TimeField};
use chrono::{DateTime, Duration, Utc};

/// The setting that turns a mode on: `off`, `window`, `recency` or `both`.
pub const FLAG_ENV: &str = "REMIND_ME_TIME_BUCKETS";
/// A window this wide or narrower is left alone.
pub const MIN_WINDOW_DAYS: i64 = 3;
const MIN_BUCKETS: i64 = 3;
const MAX_BUCKETS: i64 = 6;

/// Which spreading modes are on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modes {
    pub window: bool,
    pub recency: bool,
}

impl Modes {
    /// Every mode off.
    pub const OFF: Self = Self {
        window: false,
        recency: false,
    };

    /// Read a setting's value. Unset, blank and anything unrecognised read as
    /// off, as `REMIND_ME_DATE_PHRASES` does: a spike never turns on by
    /// accident.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "window" => Self {
                window: true,
                recency: false,
            },
            "recency" => Self {
                window: false,
                recency: true,
            },
            "both" => Self {
                window: true,
                recency: true,
            },
            _ => Self::OFF,
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

/// Apply the modes `modes` turns on to a whole ranked list. Window mode
/// needs a `since` (an open `until` is `now`); recency mode needs no window
/// at all. Anything else is returned unchanged.
pub fn apply(
    ranked: Vec<MemorySearchResult>,
    modes: Modes,
    window: &crate::time_window::TimeWindow,
    limit: usize,
    now: DateTime<Utc>,
) -> Vec<MemorySearchResult> {
    let field = window.field();
    let instant = |r: &MemorySearchResult| instant_of(&r.memory, field);
    match window.bounds() {
        (Some(since), until) if modes.window => {
            spread_window(ranked, since, until.unwrap_or(now), instant)
        }
        (None, None) if modes.recency => reserve_recent(ranked, limit, now, instant),
        _ => ranked,
    }
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

/// The equal buckets a `[since, until)` window is cut into, newest first, or
/// none when it is not wider than [`MIN_WINDOW_DAYS`].
fn window_buckets(since: DateTime<Utc>, until: DateTime<Utc>) -> Vec<Bucket> {
    let span = until - since;
    if span <= Duration::days(MIN_WINDOW_DAYS) {
        return Vec::new();
    }
    let count = (span.num_days() / 7).clamp(MIN_BUCKETS, MAX_BUCKETS);
    let width = span / count as i32;
    (0..count)
        .rev()
        .map(|i| {
            let start = since + width * i as i32;
            let end = if i == count - 1 { until } else { start + width };
            Bucket {
                start: Some(start),
                end: Some(end),
            }
        })
        .collect()
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

/// Window mode: interleave `ranked` across the buckets of `[since, until)`,
/// newest bucket first, keeping rank order inside each; empty buckets are
/// skipped. Unchanged when the window is too narrow.
pub fn spread_window<T>(
    ranked: Vec<T>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    instant: impl Fn(&T) -> Option<DateTime<Utc>>,
) -> Vec<T> {
    let buckets = window_buckets(since, until);
    if buckets.is_empty() {
        return ranked;
    }
    let slots = assign(&ranked, &buckets, &instant);
    let mut queues: Vec<Vec<T>> = (0..buckets.len()).map(|_| Vec::new()).collect();
    for (item, slot) in ranked.into_iter().zip(slots) {
        queues[slot].push(item);
    }
    round_robin(queues)
}

fn round_robin<T>(queues: Vec<Vec<T>>) -> Vec<T> {
    let total = queues.iter().map(Vec::len).sum();
    let mut iters: Vec<std::vec::IntoIter<T>> = queues.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        for it in iters.iter_mut() {
            if let Some(item) = it.next() {
                out.push(item);
            }
        }
    }
    out
}

/// Recency mode: make sure the best item of every non-empty recency bucket is
/// in the first `limit`, displacing the lowest-ranked items of buckets that
/// already have more than one there. Everything keeps its rank order.
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
    for bucket in 0..buckets.len() {
        let represented = (0..ranked.len()).any(|i| chosen[i] && slots[i] == bucket);
        let Some(best) = (0..ranked.len()).find(|&i| !chosen[i] && slots[i] == bucket) else {
            continue;
        };
        if represented {
            continue;
        }
        let crowded = |i: usize| {
            (0..ranked.len())
                .filter(|&j| chosen[j] && slots[j] == slots[i])
                .count()
                > 1
        };
        let Some(drop) = (0..ranked.len()).rev().find(|&i| chosen[i] && crowded(i)) else {
            continue;
        };
        chosen[drop] = false;
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
    fn modes_parse_and_default_off() {
        assert_eq!(Modes::parse(""), Modes::OFF);
        assert_eq!(Modes::parse("off"), Modes::OFF);
        assert_eq!(Modes::parse("on"), Modes::OFF);
        assert_eq!(
            Modes::parse(" Window "),
            Modes {
                window: true,
                recency: false
            }
        );
        assert_eq!(
            Modes::parse("recency"),
            Modes {
                window: false,
                recency: true
            }
        );
        assert_eq!(
            Modes::parse("BOTH"),
            Modes {
                window: true,
                recency: true
            }
        );
    }

    #[test]
    fn a_narrow_window_is_unchanged() {
        let ranked = vec![("a", 1), ("b", 1), ("c", 2)];
        let since = now() - Duration::days(3);
        assert_eq!(
            names(spread_window(ranked, since, now(), at)),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn a_wide_window_interleaves_newest_bucket_first() {
        // 30 days: 4 buckets of 7.5 days. Three hits on day 28, one on day 2.
        let ranked = vec![("old1", 28), ("old2", 28), ("old3", 28), ("new", 2)];
        let since = now() - Duration::days(30);
        assert_eq!(
            names(spread_window(ranked, since, now(), at)),
            ["new", "old1", "old2", "old3"]
        );
    }

    #[test]
    fn empty_buckets_are_skipped_and_rank_order_holds_inside_a_bucket() {
        let ranked = vec![("a", 25), ("b", 1), ("c", 25), ("d", 1), ("e", 12)];
        let since = now() - Duration::days(28);
        // 28 days: 4 buckets of 7 days; 25 is the oldest, 12 the second
        // oldest, 1 the newest; the bucket 7-14 days old... holds e only.
        assert_eq!(
            names(spread_window(ranked, since, now(), at)),
            ["b", "e", "a", "d", "c"]
        );
    }

    #[test]
    fn bucket_count_is_clamped() {
        assert_eq!(window_buckets(now() - Duration::days(4), now()).len(), 3);
        assert_eq!(window_buckets(now() - Duration::days(35), now()).len(), 5);
        assert_eq!(window_buckets(now() - Duration::days(365), now()).len(), 6);
        let b = window_buckets(now() - Duration::days(365), now());
        assert_eq!(b[0].end, Some(now()));
        assert_eq!(b[5].start, Some(now() - Duration::days(365)));
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
