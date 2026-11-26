//! The `since`/`until` window a search narrows to.
//!
//! Each bound is RFC 3339, a relative age (`30m`, `12h`, `2d`, `1w`) or a date
//! phrase (`yesterday`, `last week`, `Q3`; see [`crate::date_phrase`]) that is
//! resolved against the clock once, at the boundary, so everything downstream
//! sees absolute instants and a search serialised to the daemon means the same
//! thing when it arrives.

use crate::date_phrase::{self, Span};
use crate::kinds::OccurredAt;
use chrono::{DateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// Which timestamp a [`TimeWindow`] reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeField {
    /// When the memory was recorded.
    #[default]
    Created,
    /// When the memory was last edited.
    Updated,
    /// When its event happened: `metadata.occurred_at` (an instant or a
    /// span; see [`OccurredAt`]), else `created_at` (card #62).
    Occurred,
}

/// A half-open `[since, until)` window over one timestamp.
///
/// Half-open so back-to-back windows never count a memory twice. Only built by
/// [`TimeWindow::resolve`], so `since < until` holds whenever both are set. The
/// default is unbounded and admits everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawTimeWindow", into = "RawTimeWindow")]
pub struct TimeWindow {
    /// `None`: the caller named no field, so [`TimeField::default`] applies.
    field: Option<TimeField>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
}

/// The wire form: the three tool arguments as a caller types them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawTimeWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// `None` when the caller passed none, which reads as
    /// [`TimeField::default`]; kept apart from an explicit `created` so date
    /// inference can tell them apart (pack D93).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_field: Option<TimeField>,
}

impl TimeWindow {
    /// Validate `raw`, resolving relative ages and date phrases against `now`.
    ///
    /// Phrases are read in [`date_phrase::configured_zone`]. A `since` phrase
    /// naming a whole period (`last week`, `Q3`) also sets `until` to its end
    /// when `until` is not given. Errors name the offending argument: a bound
    /// that is none of the accepted forms, an `until` phrase with no end
    /// (`past 3 days`), or a window that ends at or before it starts.
    pub fn resolve(raw: &RawTimeWindow, now: DateTime<Utc>) -> Result<Self, String> {
        Self::resolve_with(raw, now, date_phrase::configured_zone)
    }

    /// [`Self::resolve`] with the zone injected; it is only asked for when a
    /// bound is a phrase, so a bad zone setting never fails an RFC 3339 bound.
    fn resolve_with(
        raw: &RawTimeWindow,
        now: DateTime<Utc>,
        zone: impl Fn() -> Result<Tz, String>,
    ) -> Result<Self, String> {
        let bound = |name: &str, value: &Option<String>| {
            value
                .as_deref()
                .map(|v| parse_bound(v, now, &zone).map_err(|e| format!("`{name}`: {e}")))
                .transpose()
        };
        let (since, implied_until) = match bound("since", &raw.since)? {
            None => (None, None),
            Some(Bound::At(at)) => (Some(at), None),
            Some(Bound::Span(span)) => (Some(span.start), span.end),
        };
        let until = match bound("until", &raw.until)? {
            None => implied_until,
            Some(Bound::At(at)) => Some(at),
            Some(Bound::Span(Span { end: Some(end), .. })) => Some(end),
            Some(Bound::Span(Span { end: None, .. })) => {
                return Err(format!(
                    "`until`: {:?} runs up to now and has no end; pass it as `since`",
                    raw.until.as_deref().unwrap_or_default()
                ))
            }
        };
        if let (Some(s), Some(u)) = (since, until) {
            if u <= s {
                return Err(format!(
                    "`until` ({}) must be after `since` ({})",
                    u.to_rfc3339(),
                    s.to_rfc3339()
                ));
            }
        }
        Ok(Self {
            field: raw.time_field,
            since,
            until,
        })
    }

    /// Which timestamp this window reads.
    pub fn field(&self) -> TimeField {
        self.field.unwrap_or_default()
    }

    /// The field the caller named, if any.
    pub fn explicit_field(&self) -> Option<TimeField> {
        self.field
    }

    /// The window a date phrase's span names, over `field`.
    ///
    /// A span always starts before it ends, so the `since < until` invariant
    /// holds without a check.
    pub fn from_span(field: TimeField, span: Span) -> Self {
        Self {
            field: Some(field),
            since: Some(span.start),
            until: span.end,
        }
    }

    /// No bound set, so every memory is inside.
    pub fn is_unbounded(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }

    /// Whether a memory with these timestamps falls inside: an instant inside
    /// `[since, until)`, or a span that overlaps it. `occurred` gives the
    /// memory's `metadata.occurred_at` and is called only for
    /// [`TimeField::Occurred`]. A bounded window cannot place an unparseable
    /// timestamp, so it excludes it.
    pub fn contains(
        &self,
        created_at: &str,
        updated_at: &str,
        occurred: impl FnOnce() -> Option<OccurredAt>,
    ) -> bool {
        if self.is_unbounded() {
            return true;
        }
        let after_since = |at: DateTime<Utc>| self.since.is_none_or(|s| at > s);
        let before_until = |at: DateTime<Utc>| self.until.is_none_or(|u| at < u);
        match self.place(created_at, updated_at, occurred) {
            None => false,
            Some(OccurredAt::At(at)) => self.since.is_none_or(|s| at >= s) && before_until(at),
            Some(OccurredAt::Span { from, to }) => before_until(from) && after_since(to),
        }
    }

    /// [`Self::contains`] over a [`crate::models::Memory`].
    pub fn contains_memory(&self, m: &crate::models::Memory) -> bool {
        self.contains(&m.created_at, &m.updated_at, || OccurredAt::of(&m.metadata))
    }

    /// The instant this window orders a memory by, newest first: the
    /// timestamp it reads, or a span's `to`. `None` when unparseable.
    pub(crate) fn sort_key(
        &self,
        created_at: &str,
        updated_at: &str,
        occurred: impl FnOnce() -> Option<OccurredAt>,
    ) -> Option<DateTime<Utc>> {
        self.place(created_at, updated_at, occurred)
            .map(OccurredAt::latest)
    }

    /// Where this window's field puts a memory on the timeline.
    fn place(
        &self,
        created_at: &str,
        updated_at: &str,
        occurred: impl FnOnce() -> Option<OccurredAt>,
    ) -> Option<OccurredAt> {
        let at = |ts: &str| parse_rfc3339(ts).map(OccurredAt::At);
        match self.field() {
            TimeField::Created => at(created_at),
            TimeField::Updated => at(updated_at),
            TimeField::Occurred => occurred().or_else(|| at(created_at)),
        }
    }
}

impl TryFrom<RawTimeWindow> for TimeWindow {
    type Error = String;

    fn try_from(raw: RawTimeWindow) -> Result<Self, String> {
        Self::resolve(&raw, Utc::now())
    }
}

impl From<TimeWindow> for RawTimeWindow {
    fn from(w: TimeWindow) -> Self {
        Self {
            since: w.since.map(|t| t.to_rfc3339()),
            until: w.until.map(|t| t.to_rfc3339()),
            time_field: w.field,
        }
    }
}

/// Parse an RFC 3339 timestamp to UTC.
pub fn parse_rfc3339(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// One parsed `since`/`until` value: an instant, or a phrase's span.
enum Bound {
    At(DateTime<Utc>),
    Span(Span),
}

fn parse_bound(
    value: &str,
    now: DateTime<Utc>,
    zone: impl Fn() -> Result<Tz, String>,
) -> Result<Bound, String> {
    let value = value.trim();
    if let Some(at) = parse_rfc3339(value) {
        return Ok(Bound::At(at));
    }
    if let Some(age) = parse_age(value) {
        return now
            .checked_sub_signed(age)
            .map(Bound::At)
            .ok_or_else(|| format!("{value:?} reaches before the earliest representable time"));
    }
    date_phrase::parse(value, now.with_timezone(&zone()?))
        .map(Bound::Span)
        .ok_or_else(|| {
            format!(
                "{value:?} is not RFC 3339, an age like 30m, 12h, 2d or 1w, \
                 or a date phrase like yesterday, last week, Q3 or 2026-10"
            )
        })
}

/// `<n>m|h|d|w` with `n > 0`.
fn parse_age(value: &str) -> Option<TimeDelta> {
    let unit = value.chars().last()?;
    let n: i64 = value[..value.len() - unit.len_utf8()].parse().ok()?;
    if n <= 0 {
        return None;
    }
    match unit {
        'm' => TimeDelta::try_minutes(n),
        'h' => TimeDelta::try_hours(n),
        'd' => TimeDelta::try_days(n),
        'w' => TimeDelta::try_weeks(n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        parse_rfc3339("2026-10-26T12:00:00Z").unwrap()
    }

    fn in_utc(r: &RawTimeWindow) -> Result<TimeWindow, String> {
        TimeWindow::resolve_with(r, now(), || Ok(Tz::UTC))
    }

    fn at(ts: &str) -> Option<DateTime<Utc>> {
        parse_rfc3339(ts)
    }

    fn raw(since: Option<&str>, until: Option<&str>) -> RawTimeWindow {
        RawTimeWindow {
            since: since.map(str::to_string),
            until: until.map(str::to_string),
            time_field: None,
        }
    }

    #[test]
    fn relative_ages_resolve_against_now() {
        for (age, expected) in [
            ("30m", "2026-10-26T11:30:00Z"),
            ("12h", "2026-10-26T00:00:00Z"),
            ("2d", "2026-10-24T12:00:00Z"),
            ("1w", "2026-10-19T12:00:00Z"),
        ] {
            let w = TimeWindow::resolve(&raw(Some(age), None), now()).unwrap();
            assert_eq!(w.since, parse_rfc3339(expected), "{age}");
        }
    }

    #[test]
    fn rfc3339_offsets_normalise_to_utc() {
        let w = TimeWindow::resolve(&raw(Some("2026-10-26T14:00:00+02:00"), None), now()).unwrap();
        assert_eq!(w.since, Some(now()));
    }

    #[test]
    fn bad_input_is_rejected_naming_the_argument() {
        for (r, needle) in [
            (raw(Some("soon"), None), "`since`"),
            (raw(None, Some("2x")), "`until`"),
            (raw(None, Some("past 3 days")), "has no end"),
            (raw(Some("past 2 days"), Some("past 3 days")), "`until`"),
            (raw(Some("0d"), None), "`since`"),
            (raw(Some("-2d"), None), "`since`"),
            (raw(Some("d"), None), "`since`"),
            (raw(Some(""), None), "`since`"),
            (raw(Some("99999999999w"), None), "`since`"),
        ] {
            let err = in_utc(&r).unwrap_err();
            assert!(err.contains(needle), "{err}");
        }
    }

    #[test]
    fn until_must_follow_since() {
        let err = TimeWindow::resolve(&raw(Some("1d"), Some("2d")), now()).unwrap_err();
        assert!(err.contains("must be after"), "{err}");
        let at = "2026-10-26T00:00:00Z";
        assert!(TimeWindow::resolve(&raw(Some(at), Some(at)), now()).is_err());
    }

    #[test]
    fn window_is_half_open() {
        let w = TimeWindow::resolve(
            &raw(Some("2026-10-25T00:00:00Z"), Some("2026-10-26T00:00:00Z")),
            now(),
        )
        .unwrap();
        let has = |ts: &str| w.contains(ts, "", || None);
        assert!(has("2026-10-25T00:00:00Z"), "since is inclusive");
        assert!(has("2026-10-25T23:59:59.999Z"));
        assert!(!has("2026-10-26T00:00:00Z"), "until is exclusive");
        assert!(!has("2026-10-24T23:59:59Z"));
        assert!(!has("not a timestamp"));
    }

    #[test]
    fn time_field_picks_the_timestamp() {
        let mut r = raw(Some("1d"), None);
        r.time_field = Some(TimeField::Updated);
        let w = TimeWindow::resolve(&r, now()).unwrap();
        let old = "2026-01-01T00:00:00Z";
        let new = "2026-10-26T11:00:00Z";
        assert!(w.contains(old, new, || None));
        assert!(!w.contains(new, old, || None));
    }

    #[test]
    fn occurred_reads_occurred_at_else_created_at() {
        let mut r = raw(Some("2026-10-01T00:00:00Z"), Some("2026-10-08T00:00:00Z"));
        r.time_field = Some(TimeField::Occurred);
        let w = in_utc(&r).unwrap();
        let point = |ts: &str| Some(OccurredAt::At(at(ts).unwrap()));
        let written = "2026-10-20T00:00:00Z";
        assert!(w.contains(written, written, || point("2026-10-03T00:00:00Z")));
        assert!(!w.contains(written, written, || point("2026-10-08T00:00:00Z")));
        // No occurred_at: created_at decides.
        assert!(w.contains("2026-10-02T00:00:00Z", written, || None));
        assert!(!w.contains(written, "2026-10-02T00:00:00Z", || None));
        assert_eq!(
            w.sort_key(written, written, || point("2026-10-03T00:00:00Z")),
            at("2026-10-03T00:00:00Z")
        );
    }

    #[test]
    fn a_span_is_inside_when_it_overlaps_the_window() {
        let mut r = raw(Some("2026-10-01T00:00:00Z"), Some("2026-10-08T00:00:00Z"));
        r.time_field = Some(TimeField::Occurred);
        let w = in_utc(&r).unwrap();
        let written = "2026-10-20T00:00:00Z";
        let overlaps = |from: &str, to: &str| {
            let span = OccurredAt::Span {
                from: at(from).unwrap(),
                to: at(to).unwrap(),
            };
            w.contains(written, written, || Some(span))
        };
        assert!(
            overlaps("2026-09-28T00:00:00Z", "2026-10-02T00:00:00Z"),
            "across since"
        );
        assert!(
            overlaps("2026-10-07T00:00:00Z", "2026-10-12T00:00:00Z"),
            "across until"
        );
        assert!(
            overlaps("2026-10-02T00:00:00Z", "2026-10-03T00:00:00Z"),
            "inside"
        );
        assert!(
            overlaps("2026-09-01T00:00:00Z", "2026-11-01T00:00:00Z"),
            "around"
        );
        assert!(
            !overlaps("2026-09-01T00:00:00Z", "2026-10-01T00:00:00Z"),
            "ends at since"
        );
        assert!(
            !overlaps("2026-10-08T00:00:00Z", "2026-10-09T00:00:00Z"),
            "starts at until"
        );
        let span = OccurredAt::Span {
            from: at("2026-09-28T00:00:00Z").unwrap(),
            to: at("2026-10-02T00:00:00Z").unwrap(),
        };
        assert_eq!(
            w.sort_key(written, written, || Some(span)),
            at("2026-10-02T00:00:00Z")
        );
    }

    #[test]
    fn created_and_updated_never_read_occurred_at() {
        let w = in_utc(&raw(Some("2026-10-01T00:00:00Z"), None)).unwrap();
        let called = std::cell::Cell::new(false);
        assert!(w.contains("2026-10-20T00:00:00Z", "", || {
            called.set(true);
            None
        }));
        assert!(!called.get());
    }

    #[test]
    fn time_field_occurred_parses() {
        let w: TimeWindow =
            serde_json::from_value(serde_json::json!({ "since": "2d", "time_field": "occurred" }))
                .unwrap();
        assert_eq!(w.field(), TimeField::Occurred);
        let back = serde_json::to_value(w).unwrap();
        assert_eq!(back["time_field"], "occurred");
        assert!(serde_json::from_value::<TimeWindow>(
            serde_json::json!({ "time_field": "happened" })
        )
        .is_err());
    }

    #[test]
    fn an_absent_time_field_is_kept_apart_from_an_explicit_one() {
        let absent: TimeWindow =
            serde_json::from_value(serde_json::json!({ "since": "2d" })).unwrap();
        assert_eq!(absent.field(), TimeField::Created);
        assert_eq!(absent.explicit_field(), None);
        assert!(serde_json::to_value(absent)
            .unwrap()
            .get("time_field")
            .is_none());
        let created: TimeWindow =
            serde_json::from_value(serde_json::json!({ "since": "2d", "time_field": "created" }))
                .unwrap();
        assert_eq!(created.explicit_field(), Some(TimeField::Created));
        let back: TimeWindow =
            serde_json::from_value(serde_json::to_value(created).unwrap()).unwrap();
        assert_eq!(back, created);
    }

    #[test]
    fn unbounded_admits_everything() {
        assert!(TimeWindow::default().contains("garbage", "garbage", || None));
    }

    #[test]
    fn serde_round_trips_resolved_instants() {
        let w: TimeWindow =
            serde_json::from_value(serde_json::json!({ "since": "2d", "time_field": "updated" }))
                .unwrap();
        assert_eq!(w.field(), TimeField::Updated);
        let back: TimeWindow = serde_json::from_value(serde_json::to_value(w).unwrap()).unwrap();
        assert_eq!(back, w);
        assert!(
            serde_json::from_value::<TimeWindow>(serde_json::json!({ "since": "soon" })).is_err()
        );
    }

    #[test]
    fn date_phrases_set_bounds_by_the_argument_rules() {
        // now() is Monday 2026-10-26 12:00 UTC.
        for (since, until, want_since, want_until) in [
            // `since` alone naming a whole period sets both bounds.
            (
                Some("yesterday"),
                None,
                "2026-10-25T00:00:00Z",
                Some("2026-10-26T00:00:00Z"),
            ),
            (
                Some("last week"),
                None,
                "2026-10-19T00:00:00Z",
                Some("2026-10-26T00:00:00Z"),
            ),
            (
                Some("Q3"),
                None,
                "2026-07-01T00:00:00Z",
                Some("2026-10-01T00:00:00Z"),
            ),
            // A rolling phrase runs up to now.
            (Some("past 3 days"), None, "2026-10-23T12:00:00Z", None),
            // Explicit arguments keep #350's leading plus; only query
            // inference refuses it.
            (Some("past +3 days"), None, "2026-10-23T12:00:00Z", None),
            // An explicit `until` wins over the period's end.
            (
                Some("last month"),
                Some("2026-09-15T00:00:00Z"),
                "2026-09-01T00:00:00Z",
                Some("2026-09-15T00:00:00Z"),
            ),
            // `until` takes a period's end.
            (
                Some("2d"),
                Some("today"),
                "2026-10-24T12:00:00Z",
                Some("2026-10-27T00:00:00Z"),
            ),
            (
                Some("2026-01"),
                Some("2026-03"),
                "2026-01-01T00:00:00Z",
                Some("2026-04-01T00:00:00Z"),
            ),
        ] {
            let w = in_utc(&raw(since, until)).unwrap();
            assert_eq!(w.since, at(want_since), "{since:?} {until:?}");
            assert_eq!(w.until, want_until.and_then(at), "{since:?} {until:?}");
        }
    }

    #[test]
    fn a_phrase_never_shifts_ages_or_instants() {
        for value in ["30m", "2d", "1w", "2026-10-26T14:00:00+02:00"] {
            let plain = in_utc(&raw(Some(value), None)).unwrap();
            let sydney = TimeWindow::resolve_with(&raw(Some(value), None), now(), || {
                Ok(chrono_tz::Australia::Sydney)
            })
            .unwrap();
            assert_eq!(plain, sydney, "{value}");
            assert_eq!(plain.until, None, "{value}");
        }
    }

    #[test]
    fn a_rolling_since_across_a_dst_change_resolves() {
        // 02:30 AEDT on 2026-10-05; a day earlier is inside the spring-forward gap.
        let now = parse_rfc3339("2026-10-04T15:30:00Z").unwrap();
        let w = TimeWindow::resolve_with(&raw(Some("past 1 day"), None), now, || {
            Ok(chrono_tz::Australia::Sydney)
        })
        .unwrap();
        assert_eq!(w.since, at("2026-10-03T16:00:00Z"));
        assert_eq!(w.until, None);
    }
}
