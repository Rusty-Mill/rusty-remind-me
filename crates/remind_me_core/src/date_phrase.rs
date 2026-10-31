//! Plain-English date phrases ("yesterday", "last month", "Q3 2026") as spans.
//!
//! Pure: no clock, no env. The caller passes `now` in the zone whose calendar
//! the phrase means, so "yesterday" is the user's yesterday, daylight saving
//! included. [`configured_zone`] is the one place that reads the zone setting.

use chrono::{
    DateTime, Datelike, Days, LocalResult, Month, Months, NaiveDate, NaiveDateTime, TimeDelta,
    TimeZone, Utc, Weekday,
};
use chrono_tz::Tz;
use std::ops::Range;

/// The IANA zone (`Australia/Sydney`) date phrases are read in; UTC if unset.
pub const TZ_ENV: &str = "REMIND_ME_TZ";

/// A half-open `[start, end)` span; `end: None` runs up to now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
}

/// The zone [`TZ_ENV`] names, or UTC when it is unset or blank.
///
/// A value that is not an IANA zone is an error, not a silent UTC: a phrase
/// read in the wrong zone would shift every window by hours.
pub fn configured_zone() -> Result<Tz, String> {
    let raw = std::env::var(TZ_ENV).unwrap_or_default();
    let name = raw.trim();
    if name.is_empty() {
        return Ok(Tz::UTC);
    }
    name.parse::<Tz>()
        .map_err(|_| format!("{TZ_ENV}={raw:?} is not an IANA time zone like Australia/Sydney"))
}

/// The span `text` names when the whole of it is a date phrase.
///
/// Calendar forms: `today`, `yesterday`, `this|last week|month|quarter|year`,
/// `[in] <month> [year]`, `Q1`–`Q4 [year]`, `YYYY-MM-DD`, `YYYY-MM`, and ranges
/// of those as `A to B`, `A..B` or `between A and B`. Rolling forms run up to
/// now: `past|last N hours|days|weeks|months|years`, `past week|month|year`,
/// `since <weekday>`, `since <month>`. A month or quarter without a year is the
/// latest one that has started. Rolling days to years keep the wall-clock time
/// and cross DST taking the earlier of a repeated time and the first time
/// after a skipped one. Anything else is `None`.
pub fn parse(text: &str, now: DateTime<Tz>) -> Option<Span> {
    let lower = text.trim().to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    rolling(&words, now)
        .or_else(|| range(&lower, &words, now.date_naive()).and_then(|p| p.span(now.timezone())))
}

/// The first relative phrase inside `query` (`yesterday`, `last week`,
/// `past 3 days`, `since monday`; no month names, quarters or ISO dates, which
/// in free text are usually the subject rather than a filter), with the byte
/// range it covers. Punctuation around a word is ignored; a phrase followed by
/// `of` or a year ("the last week of September", "since March 2025") is not
/// one, as the whole names something the relative part alone would not.
pub fn find_relative(query: &str, now: DateTime<Tz>) -> Option<(Span, Range<usize>)> {
    let tokens = tokens(query);
    let norms: Vec<String> = tokens
        .iter()
        .map(|&(a, b)| normalize_word(&query[a..b]))
        .collect();
    for i in 0..tokens.len() {
        for len in (1..=3).rev() {
            let Some(words) = norms.get(i..i + len) else {
                continue;
            };
            if norms.get(i + len).is_some_and(|next| qualifies(next)) {
                continue;
            }
            if words.iter().any(|w| !plain(w)) {
                continue;
            }
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            if let Some(span) = relative(&words, now) {
                return Some((span, tokens[i].0..tokens[i + len - 1].1));
            }
        }
    }
    None
}

/// A word after a relative phrase that makes it something else: `of` ("the
/// last week of September") or a year ("since March 2025"), which names an
/// absolute date the relative prefix would get wrong.
fn qualifies(next: &str) -> bool {
    next == "of" || four_digit_year(next).is_some()
}

/// Free text only: a word made of letters and digits. `+3`, `-3`, `.5` and
/// `3.5` are not counts a phrase in prose should be read from, though the
/// explicit `since`/`until` parser keeps accepting `past +3 days`.
fn plain(word: &str) -> bool {
    word.chars().all(char::is_alphanumeric)
}

/// `word` lowercased with enclosing punctuation dropped. A leading sign or
/// decimal point stays, so `-3` and `.5` are not read as the count `3` or `5`.
pub(crate) fn normalize_word(word: &str) -> String {
    let numeric_syntax = |c: char| matches!(c, '-' | '+' | '.');
    word.trim_start_matches(|c: char| !c.is_alphanumeric() && !numeric_syntax(c))
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// Byte ranges of the whitespace-separated words of `text`.
fn tokens(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, text.len()));
    }
    out
}

/// A relative phrase only: the rolling forms and the calendar periods named
/// from today.
fn relative(words: &[&str], now: DateTime<Tz>) -> Option<Span> {
    if let Some(span) = rolling(words, now) {
        return Some(span);
    }
    match words {
        ["today" | "yesterday"] | ["this" | "last", _] => {
            period(words, now.date_naive())?.span(now.timezone())
        }
        _ => None,
    }
}

/// A run of whole days, `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Period {
    start: NaiveDate,
    end: NaiveDate,
}

impl Period {
    fn span(self, tz: Tz) -> Option<Span> {
        Some(Span {
            start: day_start(tz, self.start)?,
            end: Some(day_start(tz, self.end)?),
        })
    }
}

/// Local midnight, or the first time that exists when a DST jump skips it.
fn day_start(tz: Tz, day: NaiveDate) -> Option<DateTime<Utc>> {
    local_instant(tz, day.and_hms_opt(0, 0, 0)?)
}

/// The instant a local wall-clock time names. In a DST fold (the time occurs
/// twice) it is the earlier one, so a window starting there loses nothing; in
/// a DST gap (it never occurs) it is the moment of the jump, the first instant
/// whose local time is after it.
fn local_instant(tz: Tz, local: NaiveDateTime) -> Option<DateTime<Utc>> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t.with_timezone(&Utc)),
        LocalResult::None => first_instant_at_or_after(tz, local),
    }
}

/// The earliest whole-second instant whose local time is at or after `local`.
///
/// Binary search on UTC seconds: local time only moves forward within a
/// day's span except at a fold, and a gap's own neighbourhood holds none.
/// Transitions fall on whole seconds, so the answer is exact.
fn first_instant_at_or_after(tz: Tz, local: NaiveDateTime) -> Option<DateTime<Utc>> {
    const MAX_OFFSET_SECS: i64 = 18 * 3600; // chrono bounds every UTC offset by this
    let wall =
        |secs: i64| DateTime::from_timestamp(secs, 0).map(|u| u.with_timezone(&tz).naive_local());
    let guess = local.and_utc().timestamp();
    // Invariant: wall(lo) < local <= wall(hi).
    let (mut lo, mut hi) = (guess - MAX_OFFSET_SECS, guess + MAX_OFFSET_SECS);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if wall(mid)? >= local {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    DateTime::from_timestamp(hi, 0)
}

fn rolling(words: &[&str], now: DateTime<Tz>) -> Option<Span> {
    let open = |start| Some(Span { start, end: None });
    match words {
        ["past" | "last", n, unit] => open(back(now, n.parse().ok().filter(|n| *n > 0)?, unit)?),
        ["past", unit] if !unit.ends_with('s') => open(back(now, 1, unit)?),
        ["since", name] => {
            let today = now.date_naive();
            let day = match (name.parse::<Weekday>(), name.parse::<Month>()) {
                (Ok(w), _) => today.checked_sub_days(Days::new(days_since(today.weekday(), w)))?,
                (_, Ok(m)) => latest_month(today, m)?,
                _ => return None,
            };
            open(day_start(now.timezone(), day)?)
        }
        _ => None,
    }
}

/// `now` moved back `n` units: hours on the clock, the rest on the local
/// calendar (the same wall-clock time `n` days or months ago), resolved
/// through DST by [`local_instant`].
fn back(now: DateTime<Tz>, n: u32, unit: &str) -> Option<DateTime<Utc>> {
    let local = now.naive_local();
    let target = match unit.strip_suffix('s').unwrap_or(unit) {
        "hour" => {
            return now
                .checked_sub_signed(TimeDelta::try_hours(i64::from(n))?)
                .map(|t| t.with_timezone(&Utc))
        }
        "day" => local.checked_sub_days(Days::new(u64::from(n)))?,
        "week" => local.checked_sub_days(Days::new(7 * u64::from(n)))?,
        "month" => local.checked_sub_months(Months::new(n))?,
        "year" => local.checked_sub_months(Months::new(n.checked_mul(12)?))?,
        _ => return None,
    };
    local_instant(now.timezone(), target)
}

fn days_since(from: Weekday, back_to: Weekday) -> u64 {
    u64::from((from.num_days_from_monday() + 7 - back_to.num_days_from_monday()) % 7)
}

/// One period, or `A to B` / `A..B` / `between A and B` joined end to end.
fn range(lower: &str, words: &[&str], today: NaiveDate) -> Option<Period> {
    let pair = if let ["between", rest @ ..] = words {
        split_on(rest, "and")
    } else if let Some((a, b)) = lower.split_once("..") {
        Some((
            a.split_whitespace().collect(),
            b.split_whitespace().collect(),
        ))
    } else {
        split_on(words, "to")
    };
    let Some((a, b)) = pair else {
        return period(words, today);
    };
    let (a, b) = (period(&a, today)?, period(&b, today)?);
    (a.start < b.end).then_some(Period {
        start: a.start,
        end: b.end,
    })
}

fn split_on<'a>(words: &[&'a str], sep: &str) -> Option<(Vec<&'a str>, Vec<&'a str>)> {
    let at = words.iter().position(|w| *w == sep)?;
    Some((words[..at].to_vec(), words[at + 1..].to_vec()))
}

fn period(words: &[&str], today: NaiveDate) -> Option<Period> {
    match words {
        ["today"] => day(today),
        ["yesterday"] => day(today.pred_opt()?),
        ["this", unit] => current(today, unit),
        ["last", unit] => current(current(today, unit)?.start.pred_opt()?, unit),
        ["in", rest @ ..] => named(rest, today).or_else(|| iso(rest)),
        _ => named(words, today).or_else(|| iso(words)),
    }
}

fn day(d: NaiveDate) -> Option<Period> {
    Some(Period {
        start: d,
        end: d.succ_opt()?,
    })
}

/// The calendar `unit` (week from Monday, month, quarter, year) holding `d`.
fn current(d: NaiveDate, unit: &str) -> Option<Period> {
    match unit {
        "week" => {
            let start =
                d.checked_sub_days(Days::new(u64::from(d.weekday().num_days_from_monday())))?;
            Some(Period {
                start,
                end: start.checked_add_days(Days::new(7))?,
            })
        }
        "month" => months(d.year(), d.month(), 1),
        "quarter" => months(d.year(), (d.month0() / 3) * 3 + 1, 3),
        "year" => months(d.year(), 1, 12),
        _ => None,
    }
}

/// `n` months from the first of `month` in `year`.
fn months(year: i32, month: u32, n: u32) -> Option<Period> {
    let start = NaiveDate::from_ymd_opt(year, month, 1)?;
    Some(Period {
        start,
        end: start.checked_add_months(Months::new(n))?,
    })
}

/// `<month> [year]` or `q1`–`q4 [year]`; no year means the latest started.
fn named(words: &[&str], today: NaiveDate) -> Option<Period> {
    let (name, year) = match words {
        [name] => (*name, None),
        [name, year] => (*name, Some(four_digit_year(year)?)),
        _ => return None,
    };
    if let Some(q) = name
        .strip_prefix('q')
        .and_then(|q| q.parse::<u32>().ok())
        .filter(|q| (1..=4).contains(q))
    {
        let first = 3 * q - 2;
        let year = match year {
            Some(y) => y,
            None if first <= today.month() => today.year(),
            None => today.year() - 1,
        };
        return months(year, first, 3);
    }
    let month = name.parse::<Month>().ok()?;
    let start = match year {
        Some(y) => NaiveDate::from_ymd_opt(y, month.number_from_month(), 1)?,
        None => latest_month(today, month)?,
    };
    months(start.year(), start.month(), 1)
}

/// The first of the latest `month` that has started by `today`.
fn latest_month(today: NaiveDate, month: Month) -> Option<NaiveDate> {
    let m = month.number_from_month();
    let year = if m <= today.month() {
        today.year()
    } else {
        today.year() - 1
    };
    NaiveDate::from_ymd_opt(year, m, 1)
}

fn four_digit_year(word: &str) -> Option<i32> {
    if word.len() != 4 || !word.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    word.parse().ok()
}

/// `YYYY-MM-DD` (that day) or `YYYY-MM` (that month).
fn iso(words: &[&str]) -> Option<Period> {
    let [word] = words else { return None };
    if word.len() == 10 {
        return day(NaiveDate::parse_from_str(word, "%Y-%m-%d").ok()?);
    }
    let (y, m) = word.split_once('-')?;
    if m.len() != 2 || !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    months(four_digit_year(y)?, m.parse().ok()?, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wednesday 2026-10-28, 09:00 in Sydney (AEDT, UTC+11).
    fn sydney() -> DateTime<Tz> {
        local(chrono_tz::Australia::Sydney, "2026-10-28T09:00:00")
    }

    fn local(tz: Tz, at: &str) -> DateTime<Tz> {
        let naive = chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S").unwrap();
        tz.from_local_datetime(&naive).single().unwrap()
    }

    fn utc(at: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// The local dates (start, end) a calendar phrase spans in `now`'s zone.
    fn dates(text: &str, now: DateTime<Tz>) -> (String, String) {
        let span = parse(text, now).unwrap_or_else(|| panic!("{text:?} did not parse"));
        let end = span.end.unwrap_or_else(|| panic!("{text:?} has no end"));
        let show = |t: DateTime<Utc>| {
            t.with_timezone(&now.timezone())
                .format("%Y-%m-%d")
                .to_string()
        };
        (show(span.start), show(end))
    }

    #[test]
    fn calendar_phrases_span_whole_local_days() {
        let now = sydney();
        for (text, start, end) in [
            ("today", "2026-10-28", "2026-10-29"),
            ("Yesterday", "2026-10-27", "2026-10-28"),
            ("this week", "2026-10-26", "2026-11-02"),
            ("last week", "2026-10-19", "2026-10-26"),
            ("this month", "2026-10-01", "2026-11-01"),
            ("last month", "2026-09-01", "2026-10-01"),
            ("this quarter", "2026-10-01", "2027-01-01"),
            ("last quarter", "2026-07-01", "2026-10-01"),
            ("this year", "2026-01-01", "2027-01-01"),
            ("last year", "2025-01-01", "2026-01-01"),
            ("in March", "2026-03-01", "2026-04-01"),
            ("november", "2025-11-01", "2025-12-01"),
            ("in October", "2026-10-01", "2026-11-01"),
            ("sep 2024", "2024-09-01", "2024-10-01"),
            ("Q3", "2026-07-01", "2026-10-01"),
            ("q4", "2026-10-01", "2027-01-01"),
            ("Q1 2025", "2025-01-01", "2025-04-01"),
            ("2026-10-26", "2026-10-26", "2026-10-27"),
            ("2024-02", "2024-02-01", "2024-03-01"),
            ("in 2024-02", "2024-02-01", "2024-03-01"),
            ("2026-01-05 to 2026-01-09", "2026-01-05", "2026-01-10"),
            ("2026-01..2026-03", "2026-01-01", "2026-04-01"),
            ("between Q1 and Q2", "2026-01-01", "2026-07-01"),
        ] {
            assert_eq!(dates(text, now), (start.into(), end.into()), "{text}");
        }
    }

    #[test]
    fn a_day_starts_at_local_midnight_not_utc() {
        let span = parse("yesterday", sydney()).unwrap();
        assert_eq!(span.start, utc("2026-10-26T13:00:00Z"));
        assert_eq!(span.end, Some(utc("2026-10-27T13:00:00Z")));
        let in_utc = parse("yesterday", sydney().with_timezone(&Tz::UTC)).unwrap();
        assert_eq!(in_utc.start, utc("2026-10-26T00:00:00Z"));
    }

    #[test]
    fn rollovers_and_edges() {
        let jan = local(Tz::UTC, "2026-01-02T08:00:00");
        assert_eq!(
            dates("last month", jan),
            ("2025-12-01".into(), "2026-01-01".into())
        );
        assert_eq!(
            dates("last week", jan),
            ("2025-12-22".into(), "2025-12-29".into())
        );
        assert_eq!(dates("Q1", jan), ("2026-01-01".into(), "2026-04-01".into()));
        assert_eq!(dates("q2", jan), ("2025-04-01".into(), "2025-07-01".into()));
        let sunday = local(Tz::UTC, "2026-11-01T23:00:00");
        assert_eq!(
            dates("this week", sunday),
            ("2026-10-26".into(), "2026-11-02".into())
        );
        let leap = local(Tz::UTC, "2028-03-01T00:30:00");
        assert_eq!(
            dates("yesterday", leap),
            ("2028-02-29".into(), "2028-03-01".into())
        );
        assert_eq!(
            dates("2028-02", leap),
            ("2028-02-01".into(), "2028-03-01".into())
        );
    }

    #[test]
    fn rolling_phrases_run_up_to_now() {
        let now = sydney();
        let start = |text: &str| {
            let span = parse(text, now).unwrap_or_else(|| panic!("{text:?}"));
            assert_eq!(span.end, None, "{text}");
            span.start
                .with_timezone(&now.timezone())
                .format("%Y-%m-%dT%H:%M")
                .to_string()
        };
        assert_eq!(start("past 3 days"), "2026-10-25T09:00");
        // As on #350: the explicit parser takes a leading plus.
        assert_eq!(start("past +3 days"), "2026-10-25T09:00");
        assert_eq!(start("last 24 hours"), "2026-10-27T09:00");
        assert_eq!(start("past week"), "2026-10-21T09:00");
        assert_eq!(start("past 2 weeks"), "2026-10-14T09:00");
        assert_eq!(start("past month"), "2026-09-28T09:00");
        assert_eq!(start("past year"), "2025-10-28T09:00");
        assert_eq!(start("since Monday"), "2026-10-26T00:00");
        assert_eq!(
            start("since wed"),
            "2026-10-28T00:00",
            "the same weekday is today"
        );
        assert_eq!(start("since Thursday"), "2026-10-22T00:00");
        assert_eq!(start("since june"), "2026-06-01T00:00");
        assert_eq!(start("since december"), "2025-12-01T00:00");
        let march_end = local(Tz::UTC, "2026-03-31T12:00:00");
        let span = parse("past month", march_end).unwrap();
        assert_eq!(
            span.start,
            utc("2026-02-28T12:00:00Z"),
            "clamped to the month's end"
        );
    }

    #[test]
    fn a_dst_gap_at_midnight_starts_the_day_at_its_first_hour() {
        // Santiago skips 00:00-01:00 when its clocks go forward (2026-09-06).
        let tz = chrono_tz::America::Santiago;
        let now = local(tz, "2026-09-06T12:00:00");
        let span = parse("today", now).unwrap();
        assert_eq!(
            span.start.with_timezone(&tz).format("%H:%M").to_string(),
            "01:00"
        );
        // Sydney's switch to daylight time makes 2026-10-04 a 23-hour day.
        let sydney = chrono_tz::Australia::Sydney;
        let span = parse("2026-10-04", local(sydney, "2026-10-28T09:00:00")).unwrap();
        assert_eq!(span.end.unwrap() - span.start, TimeDelta::hours(23));
    }

    #[test]
    fn rolling_targets_in_a_dst_gap_or_fold_still_resolve() {
        let sydney = chrono_tz::Australia::Sydney;
        let start = |now: &str, text: &str| {
            parse(text, utc(now).with_timezone(&sydney))
                .unwrap_or_else(|| panic!("{text:?} at {now}"))
                .start
        };
        // 2026-10-04 02:00-03:00 AEST never happens: the first time after it.
        assert_eq!(
            start("2026-10-04T15:30:00Z", "past 1 day"),
            utc("2026-10-03T16:00:00Z")
        );
        assert_eq!(
            start("2026-11-03T15:30:00Z", "past month"),
            utc("2026-10-03T16:00:00Z")
        );
        assert_eq!(
            start("2026-10-10T15:30:00Z", "past week"),
            utc("2026-10-03T16:00:00Z")
        );
        // 2026-04-05 02:00-03:00 happens twice: the earlier, still in AEDT.
        assert_eq!(
            start("2026-04-05T16:30:00Z", "past 1 day"),
            utc("2026-04-04T15:30:00Z")
        );
        assert_eq!(
            start("2027-04-04T16:30:00Z", "past year"),
            utc("2026-04-04T15:30:00Z")
        );
    }

    #[test]
    fn a_gap_resolves_to_the_jump_whatever_the_minutes_and_seconds() {
        let sydney = chrono_tz::Australia::Sydney;
        let start =
            |now: &str, text: &str| parse(text, utc(now).with_timezone(&sydney)).unwrap().start;
        // 02:37 and 02:59:59.5 AEDT on Oct 5: a day back is inside Oct 4's
        // 02:00-03:00 gap, so the window starts at the jump, 03:00 AEDT.
        for now in [
            "2026-10-04T15:37:00Z",
            "2026-10-04T15:52:42Z",
            "2026-10-04T15:59:59.5Z",
            "2026-10-04T15:00:00Z",
        ] {
            assert_eq!(
                start(now, "past 1 day"),
                utc("2026-10-03T16:00:00Z"),
                "{now}"
            );
        }
        // 03:00:01 is just past the gap: the same wall-clock time a day back.
        assert_eq!(
            start("2026-10-04T16:00:01Z", "past 1 day"),
            utc("2026-10-03T16:00:01Z")
        );
        // A fold keeps its minutes and seconds, on the earlier pass.
        assert_eq!(
            start("2026-04-05T16:37:42Z", "past 1 day"),
            utc("2026-04-04T15:37:42Z")
        );
        // Hours are elapsed time, gap or not.
        assert_eq!(
            start("2026-10-04T15:37:00Z", "last 24 hours"),
            utc("2026-10-03T15:37:00Z")
        );
    }

    #[test]
    fn find_relative_locates_the_first_relative_phrase() {
        let now = local(Tz::UTC, "2026-10-28T09:00:00");
        for (query, phrase) in [
            ("deploy decisions last week", "last week"),
            ("What happened yesterday?", "yesterday?"),
            ("incidents in the past 3 days, please", "past 3 days,"),
            ("firmware since Monday", "since Monday"),
            ("TODAY standup", "TODAY"),
            ("last week and yesterday", "last week"),
            ("app crashes last 24 hours", "last 24 hours"),
            ("outages (past 3 days)", "(past 3 days)"),
        ] {
            let (_, range) = find_relative(query, now).unwrap_or_else(|| panic!("{query:?}"));
            assert_eq!(&query[range], phrase, "{query:?}");
        }
        let (span, _) = find_relative("what happened yesterday", now).unwrap();
        assert_eq!(Some(span), parse("yesterday", now));
    }

    #[test]
    fn find_relative_skips_subjects_and_near_misses() {
        let now = local(Tz::UTC, "2026-10-28T09:00:00");
        for query in [
            "Q3 roadmap",
            "the 2026-10-26 outage",
            "what changed in March",
            "march 2026 billing",
            "the last week of September",
            "this weekend",
            "weekly report",
            "pricing recently",
            "last mile delivery",
            "since monday's outage",
            "deploys since March 2025",
            "since june 2024, the backlog",
            "last week 2025",
            "deploys past -3 days",
            "deploys past .5 days",
            "deploys past 3.5 days",
            "deploys past +3 days",
            "deploys last (-3) days",
            "",
        ] {
            assert_eq!(find_relative(query, now), None, "{query:?}");
        }
    }

    #[test]
    fn near_misses_are_not_phrases() {
        let now = sydney();
        for text in [
            "",
            "weekly",
            "this weekend",
            "the last week of September",
            "last weeks",
            "ask may",
            "may said",
            "Q5",
            "q0",
            "2026-13-01",
            "2026-02-30",
            "2026-1",
            "26-10",
            "2026",
            "7",
            "past 0 days",
            "past -1 days",
            "past .5 days",
            "past 3.5 days",
            "past 3 fortnights",
            "past days",
            "since soon",
            "2026-03 to 2026-01",
            "between Q1",
            "march 26",
            "recently",
        ] {
            assert_eq!(parse(text, now), None, "{text:?} parsed");
        }
    }
}
