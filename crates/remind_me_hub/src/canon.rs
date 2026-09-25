//! Timestamp and JSON-field canonicalisation.
//!
//! This module is the hub's half of a contract it does not own. The reference
//! stores timestamps as canonical UTC ISO-8601 TEXT with `COLLATE "C"`, so that
//! *string* comparison is a correct chronological ordering — which is what
//! makes the keyset pull cursors work at all. Every timestamp that enters the
//! hub is normalised through [`canon_ts`] first, exactly as
//! `remind_me_mcp.sync._canon_ts` and `hub/main.py::_canon_ts` do.
//!
//! Getting this wrong is not a formatting nit. A record stored as
//! `2026-08-05T12:00:00Z` sorts *before* one stored as `2026-08-05T11:00:00+00:00`
//! under byte comparison despite being an hour later, so a single
//! non-canonical write can make later records permanently invisible to a
//! cursor that has already advanced past them.

use serde_json::Value;

/// Normalise a timestamp to canonical UTC ISO-8601 (`...+00:00`).
///
/// Matches Python's `datetime.isoformat()` output for a UTC-aware datetime,
/// which is what the reference writes and what every client cursor compares
/// against:
///
/// - no fractional part when microseconds are zero,
/// - exactly six digits when they are not,
/// - always the `+00:00` suffix, never `Z`.
pub fn canon_ts(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("not a timestamp: {value:?}"));
    }

    // `fromisoformat` accepts a naive timestamp and assumes UTC; so do we.
    // Offset-aware values are converted to UTC rather than rejected.
    let parsed = chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|naive| naive.and_utc())
        })
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f")
                .map(|naive| naive.and_utc())
        })
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight is a valid time"))
                .map(|naive| naive.and_utc())
        })
        .map_err(|_| format!("not a timestamp: {value:?}"))?;

    Ok(format_canonical(parsed))
}

/// Render a UTC instant the way Python's `datetime.isoformat()` does.
///
/// Split out because "now" needs the same shape as a parsed value — the
/// entity-enrichment path in `push` writes a fresh `updated_at`, and a `now`
/// formatted differently from every stored timestamp would sort wrong.
pub fn format_canonical(dt: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Timelike;
    // Sub-second zero, not "a whole number of seconds": these are already
    // whole seconds by construction, and what decides the format is whether
    // there is any fraction to print.
    if dt.nanosecond() == 0 {
        dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        dt.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

/// The current instant, canonically formatted.
pub fn now_canonical() -> String {
    format_canonical(chrono::Utc::now())
}

/// Coerce a `tags`/`metadata`/`aliases` field to something JSON-storable.
///
/// Clients have historically sent these both as real JSON and as a JSON
/// *string*, so a string is parsed rather than stored as a scalar. An
/// unparseable string falls back to the default instead of failing the record:
/// losing a malformed tag list is better than rejecting the memory it was
/// attached to. Mirrors `hub/main.py::_coerce_json_field`.
pub fn coerce_json_field(value: Option<&Value>, default: Value) -> Value {
    match value {
        None | Some(Value::Null) => default,
        Some(Value::String(raw)) => serde_json::from_str::<Value>(raw).unwrap_or(default),
        Some(other) => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_z_suffix_becomes_the_canonical_offset() {
        // The case that motivates the whole module: `Z` and `+00:00` are the
        // same instant but different bytes, and the cursors compare bytes.
        assert_eq!(
            canon_ts("2026-08-05T12:00:00Z").unwrap(),
            "2026-08-05T12:00:00+00:00"
        );
    }

    #[test]
    fn a_non_utc_offset_is_converted_not_preserved() {
        assert_eq!(
            canon_ts("2026-08-05T14:00:00+02:00").unwrap(),
            "2026-08-05T12:00:00+00:00"
        );
    }

    #[test]
    fn a_naive_timestamp_is_assumed_utc() {
        assert_eq!(
            canon_ts("2026-08-05T12:00:00").unwrap(),
            "2026-08-05T12:00:00+00:00"
        );
    }

    #[test]
    fn zero_microseconds_are_omitted_and_nonzero_are_six_digits() {
        // Python prints no fractional part when it is zero, and exactly six
        // digits otherwise. Mixed precision still orders correctly bytewise,
        // but only if we match the reference's choice at each precision.
        assert_eq!(
            canon_ts("2026-08-05T12:00:00.000000Z").unwrap(),
            "2026-08-05T12:00:00+00:00"
        );
        assert_eq!(
            canon_ts("2026-08-05T12:00:00.123456Z").unwrap(),
            "2026-08-05T12:00:00.123456+00:00"
        );
    }

    #[test]
    fn canonical_output_orders_chronologically_as_bytes() {
        // The property the whole convention exists for.
        let earlier = canon_ts("2026-08-05T11:00:00+00:00").unwrap();
        let later = canon_ts("2026-08-05T12:00:00Z").unwrap();
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn empty_and_junk_are_rejected() {
        assert!(canon_ts("").is_err());
        assert!(canon_ts("   ").is_err());
        assert!(canon_ts("not a date").is_err());
    }

    #[test]
    fn a_json_string_field_is_parsed_not_stored_as_a_scalar() {
        assert_eq!(
            coerce_json_field(
                Some(&Value::String("[\"a\",\"b\"]".into())),
                Value::Array(vec![])
            ),
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn an_unparseable_string_falls_back_rather_than_failing() {
        assert_eq!(
            coerce_json_field(Some(&Value::String("{oops".into())), Value::Array(vec![])),
            serde_json::json!([])
        );
    }

    #[test]
    fn null_and_missing_both_take_the_default() {
        assert_eq!(
            coerce_json_field(None, serde_json::json!({})),
            serde_json::json!({})
        );
        assert_eq!(
            coerce_json_field(Some(&Value::Null), serde_json::json!({})),
            serde_json::json!({})
        );
    }
}
