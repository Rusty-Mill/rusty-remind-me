//! Typed memory kinds and the structured fields a writer may set on them.
//!
//! `memory_type` used to be a free string, so a "decision" could be stored
//! with nothing recording why. This module is the one place that says what a
//! kind requires of its `metadata`, and checks the structured v32 fields
//! (`confidence`, the validity window, `outcome`) at the boundary. Pure: no
//! store access, so every rule is unit-tested here.

use crate::db::StoreError;
use chrono::{DateTime, Utc};
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
pub const OUTCOMES: [&str; 5] = ["done", "abandoned", "reverted", "superseded", "carded"];

/// What a decision's follow-up review may find (`metadata.review.outcome`).
/// Distinct from [`OUTCOMES`]: those say whether it was carried out, these
/// whether it held up.
pub const REVIEW_OUTCOMES: [&str; 3] = ["held", "revised", "reverted"];

/// A decision's `metadata.stakes`. `low` is never scheduled for review.
pub const STAKES: [&str; 2] = ["low", "normal"];

/// The `status` values an action item's metadata may hold.
pub const ACTION_STATUSES: [&str; 3] = ["open", "done", "dropped"];

/// Where a memory's content came from (`metadata.basis`, Mill Board #1553).
/// `written_by` says who wrote it; this says whether someone said it or a
/// model concluded it. Unset on a memory written before it existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Someone said it.
    Stated,
    /// A model concluded it from other memories (`metadata.premises`).
    Inferred,
    /// Seen across at least two sources (`metadata.premises`).
    Pattern,
}

impl Basis {
    pub const ALL: [Basis; 3] = [Self::Stated, Self::Inferred, Self::Pattern];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stated => "stated",
            Self::Inferred => "inferred",
            Self::Pattern => "pattern",
        }
    }

    /// The fewest `metadata.premises` this basis needs.
    fn min_premises(self) -> usize {
        match self {
            Self::Stated => 0,
            Self::Inferred => 1,
            Self::Pattern => 2,
        }
    }

    /// How strongly a conflict is settled in this memory's favour: stated
    /// beats unset and pattern, which beat inferred. Unset ranks with
    /// pattern, so memories written before `basis` existed lose only to a
    /// stated one.
    pub fn rank(basis: Option<Self>) -> u8 {
        match basis {
            Some(Self::Stated) => 2,
            Some(Self::Pattern) | None => 1,
            Some(Self::Inferred) => 0,
        }
    }

    /// The basis `metadata` records, if a valid one.
    pub fn of(metadata: &Value) -> Option<Self> {
        let raw = metadata.get("basis")?.as_str()?;
        Self::ALL.into_iter().find(|b| b.as_str() == raw)
    }
}

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

/// Check `metadata.basis` and `metadata.premises`, on any kind: `basis` is
/// one of [`Basis::ALL`]; `inferred` needs at least one premise id and
/// `pattern` two; `stated` takes none, and neither does a memory with no
/// basis.
pub fn validate_basis(metadata: &Value) -> Result<(), KindError> {
    const K: &str = "memory";
    let basis = match str_field(metadata, K, "basis")? {
        None => None,
        Some(raw) => Some(
            Basis::ALL
                .into_iter()
                .find(|b| b.as_str() == raw)
                .ok_or_else(|| bad(K, "basis", "must be one of stated, inferred, pattern"))?,
        ),
    };
    let premises = match metadata.get("premises") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|id| id.as_str().filter(|s| !s.trim().is_empty()))
            .collect::<Option<Vec<&str>>>()
            .ok_or_else(|| bad(K, "premises", "must be an array of memory ids"))?,
        Some(_) => return Err(bad(K, "premises", "must be an array of memory ids")),
    };
    let needed = basis.map_or(0, Basis::min_premises);
    // A premise named twice is still one premise.
    let distinct = premises
        .iter()
        .map(|id| id.trim())
        .collect::<std::collections::HashSet<_>>()
        .len();
    if distinct < needed {
        return Err(bad(
            K,
            "premises",
            format!(
                "needs at least {needed} memory id(s) for basis {}",
                basis.map_or("", Basis::as_str)
            ),
        ));
    }
    if !premises.is_empty() && needed == 0 {
        return Err(bad(
            K,
            "premises",
            "only applies to basis inferred or pattern",
        ));
    }
    Ok(())
}

/// What a persona line is (`metadata.kind` on a `persona` memory, #1555).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonaKind {
    /// How the assistant should act.
    Instruction,
    /// How the person likes things done.
    Preference,
    /// Something stable about the person.
    Trait,
}

impl PersonaKind {
    pub const ALL: [PersonaKind; 3] = [Self::Instruction, Self::Preference, Self::Trait];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instruction => "instruction",
            Self::Preference => "preference",
            Self::Trait => "trait",
        }
    }

    /// The prefix a brief or card line carries, e.g. `INSTRUCTION`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Instruction => "INSTRUCTION",
            Self::Preference => "PREFERENCE",
            Self::Trait => "TRAIT",
        }
    }

    /// The kind `metadata` records, if a valid one.
    pub fn of(metadata: &Value) -> Option<Self> {
        let raw = metadata.get("kind")?.as_str()?;
        Self::ALL.into_iter().find(|k| k.as_str() == raw)
    }
}

/// Whether a persona memory's `metadata.pinned` is `true` (#1555).
pub fn is_pinned(metadata: &Value) -> bool {
    metadata.get("pinned").and_then(Value::as_bool) == Some(true)
}

/// Check a `persona` memory's `metadata.pinned` (a boolean) and
/// `metadata.kind` (one of [`PersonaKind::ALL`]); both optional.
pub fn validate_persona(metadata: &Value) -> Result<(), KindError> {
    const K: &str = "persona";
    match metadata.get("pinned") {
        None | Some(Value::Null) | Some(Value::Bool(_)) => {}
        Some(_) => return Err(bad(K, "pinned", "must be a boolean")),
    }
    if let Some(raw) = str_field(metadata, K, "kind")? {
        if !PersonaKind::ALL.iter().any(|k| k.as_str() == raw) {
            return Err(bad(
                K,
                "kind",
                "must be one of instruction, preference, trait",
            ));
        }
    }
    Ok(())
}

/// Check a new `persona` memory written outside promotion: it has no
/// sources, so only a person may write one, and only pinned (pack D102).
pub fn validate_new_persona(metadata: &Value, written_by: &str) -> Result<(), KindError> {
    validate_persona(metadata)?;
    if !is_pinned(metadata) {
        return Err(bad(
            "persona",
            "pinned",
            "must be true: a persona line written by hand is a pin; promoted lines come from remind_me_promote",
        ));
    }
    if written_by != "human" {
        return Err(bad(
            "persona",
            "pinned",
            format!("can only be written by a person (written_by human), not {written_by}"),
        ));
    }
    Ok(())
}

/// Check `metadata` against what `kind` requires. Unknown keys are allowed.
pub fn validate_metadata(kind: MemoryKind, metadata: &Value) -> Result<(), KindError> {
    validate_basis(metadata)?;
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
    if let Some(stakes) = str_field(meta, K, "stakes")? {
        if !STAKES.contains(&stakes) {
            return Err(bad(K, "stakes", "must be one of low, normal"));
        }
    }
    validate_review(meta.get("review"))
}

/// `metadata.review`: `{due, outcome?, note?, reviewed_at?}`, or absent.
fn validate_review(review: Option<&Value>) -> Result<(), KindError> {
    const K: &str = "decision";
    let review = match review {
        None | Some(Value::Null) => return Ok(()),
        Some(review @ Value::Object(_)) => review,
        Some(_) => return Err(bad(K, "review", "must be an object")),
    };
    // Named `review.<key>` in errors, looked up as `<key>` in the object.
    let field = |key: &str, label: &'static str| match review.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(bad(K, label, "must be a string")),
    };
    let Some(due) = field("due", "review.due")? else {
        return Err(bad(K, "review.due", "is required: an RFC 3339 timestamp"));
    };
    check_rfc3339(K, "review.due", due)?;
    if let Some(outcome) = field("outcome", "review.outcome")? {
        if !REVIEW_OUTCOMES.contains(&outcome) {
            return Err(bad(
                K,
                "review.outcome",
                "must be one of held, revised, reverted",
            ));
        }
    }
    if let Some(at) = field("reviewed_at", "review.reviewed_at")? {
        check_rfc3339(K, "review.reviewed_at", at)?;
    }
    field("note", "review.note")?;
    Ok(())
}

/// When the event a memory records happened (`metadata.occurred_at`, card
/// #62, pack D86 and D91): "we shipped X on Oct 3" written on Oct 20 occurred
/// on Oct 3. It never replaces the record time, `created_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OccurredAt {
    /// One instant.
    At(DateTime<Utc>),
    /// A span, `from < to`.
    Span {
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
}

impl OccurredAt {
    /// The value `metadata` holds, or `None` when it holds none or a
    /// malformed one: a stored row is never revalidated, so a bad value reads
    /// as absent.
    pub fn of(metadata: &Value) -> Option<Self> {
        Self::parse(metadata.get("occurred_at")).ok().flatten()
    }

    /// [`Self::of`] over a row's metadata as stored JSON text, parsed only
    /// when the text mentions the key, so a scan over every row pays for the
    /// rows that carry one.
    pub fn of_json(metadata: &str) -> Option<Self> {
        if !metadata.contains("occurred_at") {
            return None;
        }
        let value: Value = serde_json::from_str(metadata).ok()?;
        Self::of(&value)
    }

    /// The latest instant it covers: the point, or a span's `to`.
    pub fn latest(self) -> DateTime<Utc> {
        match self {
            Self::At(at) => at,
            Self::Span { to, .. } => to,
        }
    }

    /// Parse a `metadata.occurred_at` value; `None` when absent or null.
    fn parse(value: Option<&Value>) -> Result<Option<Self>, KindError> {
        const K: &str = "memory";
        const SHAPE: &str = "must be an RFC 3339 timestamp or {from, to}";
        let instant = |field: &'static str, value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc))
                .ok_or_else(|| bad(K, field, "must be an RFC 3339 timestamp"))
        };
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(_)) => instant("occurred_at", value)
                .map(|at| Some(Self::At(at)))
                .map_err(|_| bad(K, "occurred_at", SHAPE)),
            Some(span @ Value::Object(keys)) => {
                if let Some(extra) = keys.keys().find(|k| *k != "from" && *k != "to") {
                    return Err(KindError::Field {
                        kind: K,
                        field: "occurred_at",
                        problem: format!("may hold only from and to, not `occurred_at.{extra}`"),
                    });
                }
                let from = instant("occurred_at.from", span.get("from"))?;
                let to = instant("occurred_at.to", span.get("to"))?;
                if from >= to {
                    return Err(bad(K, "occurred_at.to", "must be after occurred_at.from"));
                }
                Ok(Some(Self::Span { from, to }))
            }
            Some(_) => Err(bad(K, "occurred_at", SHAPE)),
        }
    }
}

/// Check `metadata.occurred_at`, on any kind: absent, null, an RFC 3339
/// timestamp, or `{from, to}` with both RFC 3339 and `from < to`. Called on
/// the metadata a write supplies (add, update, decompose, auto_capture), never
/// on a stored row.
pub fn validate_occurred_at(metadata: &Value) -> Result<(), KindError> {
    OccurredAt::parse(metadata.get("occurred_at")).map(|_| ())
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
        match &self.memory_type {
            Some(t) => validate_metadata(t.parse()?, metadata)?,
            None => validate_basis(metadata)?,
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
                "must be one of done, abandoned, reverted, superseded, carded",
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

// ---------------------------------------------------------------------------
// Markdown extras
// ---------------------------------------------------------------------------

/// What a memory's header line says beyond its id: reduced confidence, a
/// validity window (`expired` once it has passed) and an outcome. Empty for
/// an ordinary memory, so the reference layout is untouched for those.
pub fn header_marks(m: &crate::models::Memory, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    let mut marks = Vec::new();
    if m.confidence < 1.0 {
        marks.push(format!("confidence {:.2}", m.confidence));
    }
    if let Some(until) = &m.valid_until {
        if crate::retrieval::is_expired(now, Some(until)) {
            marks.push("expired".to_string());
        } else {
            marks.push(format!("valid until {until}"));
        }
    }
    if let Some(outcome) = &m.outcome {
        marks.push(format!("outcome: {outcome}"));
    }
    marks
}

/// The kind-specific lines of a memory, one per extra: a decision's
/// rationale and alternatives, an action item's due date and status.
pub fn kind_lines(m: &crate::models::Memory) -> Vec<String> {
    let kind = m
        .memory_type
        .as_deref()
        .and_then(|t| t.parse::<MemoryKind>().ok());
    let text = |key: &str| m.metadata.get(key).and_then(Value::as_str);
    let mut lines = Vec::new();
    match kind {
        Some(MemoryKind::Decision) => {
            if let Some(r) = text("rationale") {
                lines.push(format!("**Rationale:** {r}"));
            }
            let alternatives: Vec<&str> = m
                .metadata
                .get("alternatives")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !alternatives.is_empty() {
                lines.push(format!("**Alternatives:** {}", alternatives.join("; ")));
            }
        }
        Some(MemoryKind::ActionItem) => {
            let mut parts = Vec::new();
            if let Some(due) = text("due") {
                parts.push(format!("**Due:** {due}"));
            }
            parts.push(format!("**Status:** {}", text("status").unwrap_or("open")));
            lines.push(parts.join("  |  "));
        }
        _ => {}
    }
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn each_basis_is_accepted_with_its_premises() {
        use serde_json::json;
        for meta in [
            json!({}),
            json!({ "basis": "stated" }),
            json!({ "basis": "inferred", "premises": ["mem_a"] }),
            json!({ "basis": "pattern", "premises": ["mem_a", "mem_b"] }),
        ] {
            assert!(super::validate_basis(&meta).is_ok(), "{meta}");
            assert!(super::validate_metadata(super::MemoryKind::Fact, &meta).is_ok());
        }
    }

    #[test]
    fn a_basis_without_enough_premises_or_an_unknown_one_is_refused() {
        use serde_json::json;
        for (meta, says) in [
            (json!({ "basis": "inferred" }), "at least 1"),
            (
                json!({ "basis": "pattern", "premises": ["mem_a"] }),
                "at least 2",
            ),
            (json!({ "basis": "guessed" }), "must be one of stated"),
            (json!({ "basis": 1 }), "must be a string"),
            (
                json!({ "basis": "stated", "premises": ["mem_a"] }),
                "only applies",
            ),
            (json!({ "premises": ["mem_a"] }), "only applies"),
            (
                json!({ "basis": "pattern", "premises": ["mem_a", "mem_a"] }),
                "at least 2",
            ),
            (
                json!({ "basis": "inferred", "premises": [""] }),
                "array of memory ids",
            ),
            (
                json!({ "basis": "inferred", "premises": "mem_a" }),
                "array of memory ids",
            ),
        ] {
            let e = super::validate_basis(&meta).unwrap_err().to_string();
            assert!(e.contains(says), "{meta}: {e}");
        }
    }

    #[test]
    fn untyped_writes_still_check_the_basis() {
        let bad = serde_json::json!({ "basis": "inferred" });
        assert!(super::StructuredFields::default()
            .validate(&bad, None)
            .is_err());
    }

    #[test]
    fn stated_outranks_unset_and_pattern_which_outrank_inferred() {
        use super::Basis;
        assert!(Basis::rank(Some(Basis::Stated)) > Basis::rank(None));
        assert_eq!(Basis::rank(None), Basis::rank(Some(Basis::Pattern)));
        assert!(Basis::rank(None) > Basis::rank(Some(Basis::Inferred)));
        let meta = serde_json::json!({ "basis": "pattern" });
        assert_eq!(Basis::of(&meta), Some(Basis::Pattern));
        assert_eq!(Basis::of(&serde_json::json!({ "basis": "x" })), None);
    }

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
        assert!(fields(json!({"confidence": 0.0}))
            .validate(&m, None)
            .is_ok());
        assert!(fields(json!({"confidence": 1.0}))
            .validate(&m, None)
            .is_ok());
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
        let w =
            json!({"valid_from": "2026-02-01T00:00:00Z", "valid_until": "2026-01-01T00:00:00Z"});
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

    fn memory(kind: &str, metadata: Value) -> crate::models::Memory {
        let mut row = crate::db::memories::NewMemory::new("m", "c", "2026-01-01T00:00:00Z");
        row.memory_type = kind.to_string();
        row.metadata = metadata;
        serde_json::from_value(json!({
            "id": "m", "content": "c", "category": "general", "tags": [], "source": "manual",
            "metadata": row.metadata, "created_at": "t", "updated_at": "t",
            "capture_id": null, "subject": null, "predicate": null, "object": null,
            "superseded_by": null, "decay_rate": 0.1, "vitality": 1.0, "base_weight": 1.0,
            "access_count": 0, "accessed_at": "t", "doc_id": null, "chunk_index": null,
            "remind_at": null, "sensitive": false, "memory_type": row.memory_type,
            "status": null, "node_id": null, "client": null, "source_capture_id": null,
            "deleted_at": null
        }))
        .unwrap()
    }

    #[test]
    fn an_ordinary_memory_renders_no_extras() {
        let m = memory("fact", json!({}));
        assert!(header_marks(&m, chrono::Utc::now()).is_empty());
        assert!(kind_lines(&m).is_empty());
    }

    #[test]
    fn header_marks_show_confidence_window_and_outcome() {
        let now = chrono::Utc::now();
        let mut m = memory("decision", json!({}));
        m.confidence = 0.5;
        m.valid_until = Some("2020-01-01T00:00:00Z".into());
        m.outcome = Some("reverted".into());
        assert_eq!(
            header_marks(&m, now),
            vec!["confidence 0.50", "expired", "outcome: reverted"]
        );
        m.valid_until = Some("2099-01-01T00:00:00Z".into());
        assert!(header_marks(&m, now)[1].starts_with("valid until 2099"));
    }

    #[test]
    fn decision_and_action_item_lines_are_one_per_extra() {
        let d = memory(
            "decision",
            json!({"rationale": "one store", "alternatives": ["sqlite", "pg"]}),
        );
        assert_eq!(
            kind_lines(&d),
            vec!["**Rationale:** one store", "**Alternatives:** sqlite; pg"]
        );
        let a = memory("action_item", json!({"due": "2026-10-05T09:00:00Z"}));
        assert_eq!(
            kind_lines(&a),
            vec!["**Due:** 2026-10-05T09:00:00Z  |  **Status:** open"]
        );
        let a = memory("action_item", json!({"status": "done"}));
        assert_eq!(kind_lines(&a), vec!["**Status:** done"]);
    }

    #[test]
    fn a_malformed_kind_is_refused_by_validate() {
        let f = fields(json!({"memory_type": "decision"}));
        assert!(f.validate(&json!({}), None).is_err());
        let f = fields(json!({"memory_type": "bogus"}));
        assert!(f.validate(&json!({}), None).is_err());
    }

    #[test]
    fn occurred_at_takes_an_instant_or_a_span() {
        for meta in [
            json!({}),
            json!({ "occurred_at": null }),
            json!({ "occurred_at": "2026-10-03T00:00:00Z" }),
            json!({ "occurred_at": "2026-10-03T09:30:00+02:00" }),
            json!({ "occurred_at": {
                "from": "2026-10-01T00:00:00Z", "to": "2026-10-03T00:00:00Z"
            } }),
        ] {
            assert!(validate_occurred_at(&meta).is_ok(), "{meta}");
        }
        let at = |ts: &str| {
            DateTime::parse_from_rfc3339(ts)
                .unwrap()
                .with_timezone(&Utc)
        };
        assert_eq!(
            OccurredAt::of(&json!({ "occurred_at": "2026-10-03T09:30:00+02:00" })),
            Some(OccurredAt::At(at("2026-10-03T07:30:00Z")))
        );
        let span = OccurredAt::of(&json!({ "occurred_at": {
            "from": "2026-10-01T00:00:00Z", "to": "2026-10-03T00:00:00Z"
        } }))
        .unwrap();
        assert_eq!(span.latest(), at("2026-10-03T00:00:00Z"));
    }

    #[test]
    fn a_malformed_occurred_at_is_refused_naming_the_key() {
        for (meta, field, says) in [
            (
                json!({ "occurred_at": "last week" }),
                "`occurred_at`",
                "RFC 3339",
            ),
            (
                json!({ "occurred_at": "2026-10-03" }),
                "`occurred_at`",
                "RFC 3339",
            ),
            (
                json!({ "occurred_at": 1_759_449_600 }),
                "`occurred_at`",
                "{from, to}",
            ),
            (
                json!({ "occurred_at": ["2026-10-03T00:00:00Z"] }),
                "`occurred_at`",
                "{from, to}",
            ),
            (
                json!({ "occurred_at": { "to": "2026-10-03T00:00:00Z" } }),
                "`occurred_at.from`",
                "RFC 3339",
            ),
            (
                json!({ "occurred_at": { "from": "2026-10-03T00:00:00Z", "to": "soon" } }),
                "`occurred_at.to`",
                "RFC 3339",
            ),
            (
                json!({ "occurred_at": {
                    "from": "2026-10-03T00:00:00Z", "to": "2026-10-03T00:00:00Z"
                } }),
                "`occurred_at.to`",
                "after occurred_at.from",
            ),
            (
                json!({ "occurred_at": {
                    "from": "2026-10-04T00:00:00Z", "to": "2026-10-03T00:00:00Z"
                } }),
                "`occurred_at.to`",
                "after occurred_at.from",
            ),
        ] {
            let e = validate_occurred_at(&meta).unwrap_err().to_string();
            assert!(e.contains(field) && e.contains(says), "{meta}: {e}");
            assert_eq!(
                OccurredAt::of(&meta),
                None,
                "a stored {meta} reads as absent"
            );
        }
    }

    #[test]
    fn a_span_with_another_key_is_refused_naming_it() {
        let meta = json!({ "occurred_at": {
            "from": "2026-10-01T00:00:00Z", "to": "2026-10-03T00:00:00Z", "extra": 1
        } });
        let e = validate_occurred_at(&meta).unwrap_err().to_string();
        assert!(e.contains("`occurred_at.extra`"), "{e}");
        assert_eq!(OccurredAt::of(&meta), None);
    }

    #[test]
    fn occurred_at_in_stored_text_is_parsed_only_when_named() {
        assert_eq!(OccurredAt::of_json("{}"), None);
        assert_eq!(OccurredAt::of_json("not json"), None);
        assert_eq!(OccurredAt::of_json(r#"{"note":"occurred_at"}"#), None);
        assert!(OccurredAt::of_json(r#"{"occurred_at":"2026-10-03T00:00:00Z"}"#).is_some());
    }

    #[test]
    fn persona_metadata_checks_pin_and_kind() {
        assert!(validate_persona(&json!({})).is_ok());
        assert!(validate_persona(&json!({"pinned": false, "kind": "instruction"})).is_ok());
        assert!(validate_persona(&json!({"pinned": "true"})).is_err());
        assert!(validate_persona(&json!({"kind": 3})).is_err());
        assert!(validate_persona(&json!({"kind": "mood"})).is_err());
        assert_eq!(
            PersonaKind::of(&json!({"kind": "trait"})),
            Some(PersonaKind::Trait)
        );
        assert_eq!(PersonaKind::of(&json!({"kind": "mood"})), None);
        assert!(is_pinned(&json!({"pinned": true})));
        assert!(!is_pinned(&json!({"pinned": "true"})));
    }

    #[test]
    fn a_new_persona_line_must_be_a_pin_by_a_person() {
        let pin = json!({"pinned": true});
        assert!(validate_new_persona(&pin, "human").is_ok());
        assert!(validate_new_persona(&pin, "model").is_err());
        assert!(validate_new_persona(&pin, "model:claude").is_err());
        assert!(validate_new_persona(&json!({}), "human").is_err());
    }
}
