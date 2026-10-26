//! The `since`/`until` window a search narrows to.
//!
//! Each bound is RFC 3339 or a relative age (`30m`, `12h`, `2d`, `1w`) that is
//! resolved against the clock once, at the boundary, so everything downstream
//! sees absolute instants and a search serialised to the daemon means the same
//! thing when it arrives.

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

/// Which timestamp a [`TimeWindow`] reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeField {
    #[default]
    Created,
    Updated,
}

/// A half-open `[since, until)` window over one timestamp.
///
/// Half-open so back-to-back windows never count a memory twice. Only built by
/// [`TimeWindow::resolve`], so `since < until` holds whenever both are set. The
/// default is unbounded and admits everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawTimeWindow", into = "RawTimeWindow")]
pub struct TimeWindow {
    field: TimeField,
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
    #[serde(default)]
    pub time_field: TimeField,
}

impl TimeWindow {
    /// Validate `raw`, resolving relative ages against `now`.
    ///
    /// Errors name the offending argument: a bound that is neither RFC 3339
    /// nor a positive `<n>m|h|d|w` age, or a window that ends at or before it
    /// starts.
    pub fn resolve(raw: &RawTimeWindow, now: DateTime<Utc>) -> Result<Self, String> {
        let bound = |name: &str, value: &Option<String>| {
            value
                .as_deref()
                .map(|v| parse_bound(v, now).map_err(|e| format!("`{name}`: {e}")))
                .transpose()
        };
        let since = bound("since", &raw.since)?;
        let until = bound("until", &raw.until)?;
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
        self.field
    }

    /// No bound set, so every memory is inside.
    pub fn is_unbounded(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }

    /// Whether a memory with these timestamps falls inside. A bounded window
    /// cannot place an unparseable timestamp, so it excludes it.
    pub fn contains(&self, created_at: &str, updated_at: &str) -> bool {
        if self.is_unbounded() {
            return true;
        }
        let Some(at) = parse_rfc3339(self.pick(created_at, updated_at)) else {
            return false;
        };
        self.since.is_none_or(|s| at >= s) && self.until.is_none_or(|u| at < u)
    }

    /// [`Self::contains`] over a [`crate::models::Memory`].
    pub fn contains_memory(&self, m: &crate::models::Memory) -> bool {
        self.contains(&m.created_at, &m.updated_at)
    }

    /// The timestamp this window reads, of the two given.
    pub(crate) fn pick<'a>(&self, created_at: &'a str, updated_at: &'a str) -> &'a str {
        match self.field {
            TimeField::Created => created_at,
            TimeField::Updated => updated_at,
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

fn parse_bound(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let value = value.trim();
    if let Some(at) = parse_rfc3339(value) {
        return Ok(at);
    }
    let age = parse_age(value).ok_or_else(|| {
        format!("{value:?} is neither RFC 3339 nor a positive age like 30m, 12h, 2d or 1w")
    })?;
    now.checked_sub_signed(age)
        .ok_or_else(|| format!("{value:?} reaches before the earliest representable time"))
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

    fn raw(since: Option<&str>, until: Option<&str>) -> RawTimeWindow {
        RawTimeWindow {
            since: since.map(str::to_string),
            until: until.map(str::to_string),
            time_field: TimeField::Created,
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
            (raw(Some("yesterday"), None), "`since`"),
            (raw(None, Some("2x")), "`until`"),
            (raw(Some("0d"), None), "`since`"),
            (raw(Some("-2d"), None), "`since`"),
            (raw(Some("d"), None), "`since`"),
            (raw(Some(""), None), "`since`"),
            (raw(Some("99999999999w"), None), "`since`"),
        ] {
            let err = TimeWindow::resolve(&r, now()).unwrap_err();
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
        let has = |ts: &str| w.contains(ts, "");
        assert!(has("2026-10-25T00:00:00Z"), "since is inclusive");
        assert!(has("2026-10-25T23:59:59.999Z"));
        assert!(!has("2026-10-26T00:00:00Z"), "until is exclusive");
        assert!(!has("2026-10-24T23:59:59Z"));
        assert!(!has("not a timestamp"));
    }

    #[test]
    fn time_field_picks_the_timestamp() {
        let mut r = raw(Some("1d"), None);
        r.time_field = TimeField::Updated;
        let w = TimeWindow::resolve(&r, now()).unwrap();
        let old = "2026-01-01T00:00:00Z";
        let new = "2026-10-26T11:00:00Z";
        assert!(w.contains(old, new));
        assert!(!w.contains(new, old));
    }

    #[test]
    fn unbounded_admits_everything() {
        assert!(TimeWindow::default().contains("garbage", "garbage"));
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
}
