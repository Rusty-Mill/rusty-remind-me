//! Typed memory kinds and the structured fields a writer may set on them.
//!
//! `memory_type` used to be a free string, so a "decision" could be stored
//! with nothing recording why. This module is the one place that says what a
//! kind requires of its `metadata`, and checks the structured v32 fields
//! (`confidence`, the validity window, `outcome`) at the boundary. Pure: no
//! store access, so every rule is unit-tested here.

use crate::db::StoreError;
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;

/// The closed set of memory kinds. Spellings match the decay tables in
/// [`crate::vitality`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Decision,
    ActionItem,
    Preference,
    Fact,
    Reference,
    Insight,
    Learning,
    Blocker,
    WorkLog,
    Unclassified,
}

impl MemoryKind {
    pub const ALL: [MemoryKind; 10] = [
        Self::Decision,
        Self::ActionItem,
        Self::Preference,
        Self::Fact,
        Self::Reference,
        Self::Insight,
        Self::Learning,
        Self::Blocker,
        Self::WorkLog,
        Self::Unclassified,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::ActionItem => "action_item",
            Self::Preference => "preference",
            Self::Fact => "fact",
            Self::Reference => "reference",
            Self::Insight => "insight",
            Self::Learning => "learning",
            Self::Blocker => "blocker",
            Self::WorkLog => "work_log",
            Self::Unclassified => "unclassified",
        }
    }

    /// Whether an `outcome` means anything for this kind.
    pub fn tracks_outcome(self) -> bool {
        matches!(self, Self::Decision | Self::ActionItem)
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemoryKind {
    type Err = KindError;

    fn from_str(s: &str) -> Result<Self, KindError> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| KindError::UnknownKind(s.to_string()))
    }
}

/// The outcomes a memory can be resolved to.
pub const OUTCOMES: [&str; 4] = ["done", "abandoned", "reverted", "superseded"];

/// The `status` values an action item's metadata may hold.
pub const ACTION_STATUSES: [&str; 3] = ["open", "done", "dropped"];

/// Why a kind, a field or a metadata value was refused. Every variant names
/// the offending field so a model can fix the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindError {
    UnknownKind(String),
    /// `field` is missing, of the wrong type or out of range.
    Field {
        kind: &'static str,
        field: &'static str,
        problem: String,
    },
    /// `outcome` set on a kind that does not track one.
    OutcomeNotApplicable(String),
}

impl fmt::Display for KindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKind(s) => {
                let all: Vec<_> = MemoryKind::ALL.iter().map(|k| k.as_str()).collect();
                write!(
                    f,
                    "unknown memory_type `{s}`; expected one of {}",
                    all.join(", ")
                )
            }
            Self::Field {
                kind,
                field,
                problem,
            } => write!(f, "{kind} `{field}` {problem}"),
            Self::OutcomeNotApplicable(kind) => write!(
                f,
                "`outcome` only applies to decision and action_item memories, not {kind}"
            ),
        }
    }
}

impl std::error::Error for KindError {}

impl From<KindError> for StoreError {
    fn from(e: KindError) -> Self {
        StoreError::Invalid(e.to_string())
    }
}

fn bad(kind: &'static str, field: &'static str, problem: impl Into<String>) -> KindError {
    KindError::Field {
        kind,
        field,
        problem: problem.into(),
    }
}

fn str_field<'a>(
    meta: &'a Value,
    kind: &'static str,
    field: &'static str,
) -> Result<Option<&'a str>, KindError> {
    match meta.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(kind, field, "must be a string")),
    }
}

/// Parse an RFC 3339 timestamp, naming `field` on failure.
pub fn check_rfc3339(
    kind: &'static str,
    field: &'static str,
    value: &str,
) -> Result<(), KindError> {
    DateTime::parse_from_rfc3339(value)
        .map(|_| ())
        .map_err(|_| bad(kind, field, "must be an RFC 3339 timestamp"))
}

/// Check `metadata` against what `kind` requires. Unknown keys are allowed.
pub fn validate_metadata(kind: MemoryKind, metadata: &Value) -> Result<(), KindError> {
    match kind {
        MemoryKind::Decision => validate_decision(metadata),
        MemoryKind::ActionItem => validate_action_item(metadata),
        _ => Ok(()),
    }
}

fn validate_decision(meta: &Value) -> Result<(), KindError> {
    const K: &str = "decision";
    match str_field(meta, K, "rationale")? {
        Some(r) if !r.trim().is_empty() => {}
        _ => return Err(bad(K, "rationale", "is required: a non-empty string")),
    }
    if let Some(alts) = meta.get("alternatives").filter(|v| !v.is_null()) {
        let ok = alts
            .as_array()
            .is_some_and(|a| a.iter().all(Value::is_string));
        if !ok {
            return Err(bad(K, "alternatives", "must be an array of strings"));
        }
    }
    str_field(meta, K, "adr")?;
    Ok(())
}

fn validate_action_item(meta: &Value) -> Result<(), KindError> {
    const K: &str = "action_item";
    if let Some(due) = str_field(meta, K, "due")? {
        check_rfc3339(K, "due", due)?;
    }
    str_field(meta, K, "owner")?;
    if let Some(status) = str_field(meta, K, "status")? {
        if !ACTION_STATUSES.contains(&status) {
            return Err(bad(K, "status", "must be one of open, done, dropped"));
        }
    }
    Ok(())
}

/// The structured fields of a write, beyond the content: parsed from the
/// same tool arguments as `MemoryAddInput`/`MemoryUpdateInput`, which they
/// sit beside rather than inside so that adding them leaves every existing
/// constructor of those inputs untouched.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct StructuredFields {
    #[serde(default)]
    pub memory_type: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_until: Option<String>,
    #[serde(default)]
    pub verified_at: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
}

impl StructuredFields {
    /// Validate every set field. `metadata` is what the memory will hold and
    /// `effective_type` the kind it will have when this write does not set
    /// one (the stored kind, on an update).
    pub fn validate(
        &self,
        metadata: &Value,
        effective_type: Option<&str>,
    ) -> Result<(), KindError> {
        if let Some(t) = &self.memory_type {
            validate_metadata(t.parse()?, metadata)?;
        }
        if let Some(c) = self.confidence {
            if !(0.0..=1.0).contains(&c) {
                return Err(bad("memory", "confidence", "must be between 0.0 and 1.0"));
            }
        }
        for (field, value) in [
            ("valid_from", &self.valid_from),
            ("valid_until", &self.valid_until),
            ("verified_at", &self.verified_at),
        ] {
            if let Some(v) = value {
                check_rfc3339("memory", field, v)?;
            }
        }
        self.check_window()?;
        self.check_outcome(effective_type)
    }

    fn check_window(&self) -> Result<(), KindError> {
        let (Some(from), Some(until)) = (&self.valid_from, &self.valid_until) else {
            return Ok(());
        };
        // Both parsed by `validate` already; a failure here cannot happen.
        let (Ok(from), Ok(until)) = (
            DateTime::parse_from_rfc3339(from),
            DateTime::parse_from_rfc3339(until),
        ) else {
            return Ok(());
        };
        if until < from {
            return Err(bad("memory", "valid_until", "must not precede valid_from"));
        }
        Ok(())
    }

    fn check_outcome(&self, effective_type: Option<&str>) -> Result<(), KindError> {
        let Some(outcome) = &self.outcome else {
            return Ok(());
        };
        if !OUTCOMES.contains(&outcome.as_str()) {
            return Err(bad(
                "memory",
                "outcome",
                "must be one of done, abandoned, reverted, superseded",
            ));
        }
        let kind: MemoryKind = self
            .memory_type
            .as_deref()
            .or(effective_type)
            .unwrap_or("unclassified")
            .parse()
            .unwrap_or(MemoryKind::Unclassified);
        if kind.tracks_outcome() {
            Ok(())
        } else {
            Err(KindError::OutcomeNotApplicable(kind.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(v: Value) -> StructuredFields {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn kinds_round_trip_and_unknown_is_an_error() {
        for k in MemoryKind::ALL {
            assert_eq!(k.as_str().parse::<MemoryKind>().unwrap(), k);
        }
        let err = "nonsense".parse::<MemoryKind>().unwrap_err();
        assert!(err.to_string().contains("nonsense"));
    }

    #[test]
    fn a_decision_requires_a_rationale() {
        let k = MemoryKind::Decision;
        assert!(validate_metadata(k, &json!({"rationale": "cheaper"})).is_ok());
        for bad in [
            json!({}),
            json!({"rationale": ""}),
            json!({"rationale": 3}),
            json!(null),
        ] {
            let e = validate_metadata(k, &bad).unwrap_err();
            assert!(e.to_string().contains("rationale"), "{e}");
        }
    }

    #[test]
    fn decision_optional_fields_are_typed() {
        let k = MemoryKind::Decision;
        let ok = json!({"rationale": "r", "alternatives": ["a", "b"], "adr": "ADR-1", "x": 1});
        assert!(validate_metadata(k, &ok).is_ok());
        let e = validate_metadata(k, &json!({"rationale": "r", "alternatives": ["a", 2]}));
        assert!(e.unwrap_err().to_string().contains("alternatives"));
        let e = validate_metadata(k, &json!({"rationale": "r", "adr": 7}));
        assert!(e.unwrap_err().to_string().contains("adr"));
    }

    #[test]
    fn an_action_item_checks_due_and_status() {
        let k = MemoryKind::ActionItem;
        assert!(validate_metadata(k, &json!({})).is_ok());
        let ok = json!({"due": "2026-10-05T09:00:00Z", "owner": "me", "status": "done"});
        assert!(validate_metadata(k, &ok).is_ok());
        let e = validate_metadata(k, &json!({"due": "tomorrow"})).unwrap_err();
        assert!(e.to_string().contains("due"));
        let e = validate_metadata(k, &json!({"status": "wip"})).unwrap_err();
        assert!(e.to_string().contains("status"));
    }

    #[test]
    fn other_kinds_take_any_metadata() {
        for k in [
            MemoryKind::Preference,
            MemoryKind::Fact,
            MemoryKind::WorkLog,
        ] {
            assert!(validate_metadata(k, &json!({"anything": [1]})).is_ok());
        }
    }

    #[test]
    fn confidence_and_timestamps_are_range_checked() {
        let m = json!({});
        assert!(fields(json!({"confidence": 0.0})).validate(&m, None).is_ok());
        assert!(fields(json!({"confidence": 1.0})).validate(&m, None).is_ok());
        for c in [-0.1, 1.01] {
            let e = fields(json!({ "confidence": c }))
                .validate(&m, None)
                .unwrap_err();
            assert!(e.to_string().contains("confidence"));
        }
        let e = fields(json!({"valid_until": "soon"}))
            .validate(&m, None)
            .unwrap_err();
        assert!(e.to_string().contains("valid_until"));
        let w = json!({"valid_from": "2026-02-01T00:00:00Z", "valid_until": "2026-01-01T00:00:00Z"});
        assert!(fields(w).validate(&m, None).is_err());
    }

    #[test]
    fn outcome_needs_a_kind_that_tracks_one() {
        let m = json!({"rationale": "r"});
        let f = fields(json!({"outcome": "done", "memory_type": "decision"}));
        assert!(f.validate(&m, None).is_ok());
        // The stored kind applies when the write does not set one.
        let f = fields(json!({"outcome": "reverted"}));
        assert!(f.validate(&m, Some("action_item")).is_ok());
        let e = f.validate(&m, Some("fact")).unwrap_err();
        assert!(matches!(e, KindError::OutcomeNotApplicable(_)));
        assert!(f.validate(&m, None).is_err());
        let e = fields(json!({"outcome": "meh", "memory_type": "decision"}))
            .validate(&m, None)
            .unwrap_err();
        assert!(e.to_string().contains("outcome"));
    }

    #[test]
    fn a_malformed_kind_is_refused_by_validate() {
        let f = fields(json!({"memory_type": "decision"}));
        assert!(f.validate(&json!({}), None).is_err());
        let f = fields(json!({"memory_type": "bogus"}));
        assert!(f.validate(&json!({}), None).is_err());
    }
}
