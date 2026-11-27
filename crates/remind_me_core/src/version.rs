//! Version checks on writes, so clients cannot overwrite each other (#1487).
//!
//! A reader gets an opaque `version` for a memory, a wiki page or a project
//! record. A writer may hand it back as `if_version`; if the target changed
//! meanwhile, nothing is written and [`StoreError::Conflict`] carries the
//! current version. Leaving it out keeps last-write-wins.
//!
//! The token is computed on read and never stored (pack D119): `v1:` plus 16
//! hex characters of SHA-256 over what the write could overwrite. It is not
//! `updated_at`, which consolidation does not stamp and reminders stamp
//! without an edit, and not the revision count, which five write paths skip.
//! A value changed and changed back yields the same token; overwriting the
//! state the writer read loses nothing, so that is harmless.
//!
//! Pure: callers read the target under the transaction that writes it and
//! pass it here (D122).

use crate::db::{Result, StoreError};
use crate::models::Memory;
use serde_json::{json, Value};

/// The token's scheme, so the formula can change without a wire change.
const PREFIX: &str = "v1:";
/// Hex characters kept from the digest.
const HEX_LEN: usize = 16;
/// The `if_version` value that means "only if it does not exist yet".
pub const ABSENT: &str = "absent";

/// What a writer expects the target to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// The target exists with exactly this version.
    Version(String),
    /// The target does not exist yet.
    Absent,
}

impl Expect {
    /// Parse an `if_version` argument at the boundary. `None` or blank is no
    /// precondition. `absent` is accepted only where a write can create the
    /// target (`allow_absent`).
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for anything that is not a `v1:` token (or
    /// `absent` where allowed), before any read.
    pub fn parse(raw: Option<&str>, allow_absent: bool) -> Result<Option<Self>> {
        let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
            return Ok(None);
        };
        if raw == ABSENT && allow_absent {
            return Ok(Some(Self::Absent));
        }
        if is_token(raw) {
            return Ok(Some(Self::Version(raw.to_string())));
        }
        let expected = if allow_absent {
            "a version from a read (v1: plus 16 hex characters) or \"absent\""
        } else {
            "a version from a read (v1: plus 16 hex characters)"
        };
        Err(StoreError::Invalid(format!(
            "if_version {raw:?} is not {expected}"
        )))
    }
}

fn is_token(raw: &str) -> bool {
    raw.strip_prefix(PREFIX).is_some_and(|hex| {
        hex.len() == HEX_LEN
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

fn digest(value: &Value) -> String {
    let hex = sha256::digest(value.to_string());
    format!("{PREFIX}{}", &hex[..HEX_LEN])
}

/// A memory's version: every field `remind_me_update` can set, plus
/// `superseded_by`, which an update can clear. An array, so the order is
/// fixed whatever the JSON map ordering.
pub fn of_memory(memory: &Memory) -> String {
    digest(&json!([
        memory.content,
        memory.category,
        memory.tags,
        memory.metadata,
        memory.sensitive,
        memory.project,
        memory.memory_type,
        memory.confidence,
        memory.valid_from,
        memory.valid_until,
        memory.verified_at,
        memory.outcome,
        memory.remind_at,
        memory.superseded_by,
    ]))
}

/// A wiki page's version: its file body.
pub fn of_text(body: &str) -> String {
    digest(&json!(body))
}

/// A project record's version: its claim ids in order. Claims are
/// append-only, so every `set` or `revert` changes it.
pub fn of_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    digest(&json!(ids.into_iter().collect::<Vec<_>>()))
}

/// Check `expect` against the target's `current` version (`None` when it
/// does not exist). No precondition always passes.
///
/// # Errors
///
/// [`StoreError::Conflict`] with the current version, or [`ABSENT`] when
/// the target does not exist.
pub fn check(expect: Option<&Expect>, current: Option<&str>) -> Result<()> {
    let holds = match (expect, current) {
        (None, _) => true,
        (Some(Expect::Absent), current) => current.is_none(),
        (Some(Expect::Version(want)), Some(have)) => want == have,
        (Some(Expect::Version(_)), None) => false,
    };
    if holds {
        return Ok(());
    }
    Err(StoreError::Conflict {
        current_version: current.unwrap_or(ABSENT).to_string(),
    })
}

/// The body of a refused write, the same over MCP and REST (pack D121):
/// the ADR-0028 error shape plus the current version and, when the target
/// has one, when it last changed. `read_with` names the read to repeat.
pub fn conflict_body(current_version: &str, updated_at: Option<&str>, read_with: &str) -> Value {
    let mut body = json!({
        "error": {
            "code": "version_conflict",
            "message": "it changed since you read it; nothing was written",
            "fix": format!("read it again ({read_with}) and retry with its version"),
        },
        "current_version": current_version,
    });
    if let Some(at) = updated_at {
        body["updated_at"] = json!(at);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(content: &str) -> Memory {
        let row = crate::db::memories::NewMemory::new("m1", content, "2026-11-27T00:00:00Z");
        crate::db::engine::memories::MemoryRow::from_new(&row).to_memory()
    }

    #[test]
    fn the_conflict_body_has_the_contract_shape() {
        let body = conflict_body(
            "v1:0123456789abcdef",
            Some("2026-11-27T00:00:00Z"),
            "remind_me_get",
        );
        assert_eq!(body["error"]["code"], "version_conflict");
        assert_eq!(body["current_version"], "v1:0123456789abcdef");
        assert_eq!(body["updated_at"], "2026-11-27T00:00:00Z");
        assert!(body["error"]["fix"]
            .as_str()
            .unwrap()
            .contains("remind_me_get"));
        assert!(conflict_body("absent", None, "x")
            .get("updated_at")
            .is_none());
    }

    #[test]
    fn a_token_is_v1_and_16_lowercase_hex() {
        let v = of_text("hello");
        assert!(v.starts_with("v1:") && v.len() == 19, "{v}");
        assert!(is_token(&v));
    }

    #[test]
    fn a_memory_version_follows_writable_fields_only() {
        let a = memory("x");
        let mut b = a.clone();
        b.access_count += 5;
        b.updated_at = "2030-01-01T00:00:00Z".into();
        assert_eq!(
            of_memory(&a),
            of_memory(&b),
            "operational fields don't count"
        );
        b.tags = vec!["t".into()];
        assert_ne!(of_memory(&a), of_memory(&b));
        let mut c = a.clone();
        c.superseded_by = Some("mem_z".into());
        assert_ne!(of_memory(&a), of_memory(&c));
    }

    #[test]
    fn project_versions_follow_the_chain() {
        assert_ne!(of_ids(["a"]), of_ids(["a", "b"]));
        assert_ne!(of_ids(["a", "b"]), of_ids(["b", "a"]));
    }

    #[test]
    fn parse_accepts_tokens_and_absent_only_where_allowed() {
        let v = of_text("x");
        assert_eq!(Expect::parse(None, false).unwrap(), None);
        assert_eq!(Expect::parse(Some("  "), false).unwrap(), None);
        assert_eq!(
            Expect::parse(Some(&v), false).unwrap(),
            Some(Expect::Version(v.clone()))
        );
        assert_eq!(
            Expect::parse(Some("absent"), true).unwrap(),
            Some(Expect::Absent)
        );
        for bad in [
            "absent",
            "v1:xyz",
            "v2:0123456789abcdef",
            "v1:0123456789ABCDEF",
            "2026-01-01",
        ] {
            assert!(
                matches!(Expect::parse(Some(bad), false), Err(StoreError::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn check_passes_matches_and_refuses_the_rest() {
        let v = of_text("x");
        let w = of_text("y");
        assert!(check(None, Some(&v)).is_ok());
        assert!(check(None, None).is_ok());
        assert!(check(Some(&Expect::Version(v.clone())), Some(&v)).is_ok());
        assert!(check(Some(&Expect::Absent), None).is_ok());
        let conflict = |e: Result<()>| match e {
            Err(StoreError::Conflict { current_version }) => current_version,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            conflict(check(Some(&Expect::Version(v.clone())), Some(&w))),
            w
        );
        assert_eq!(conflict(check(Some(&Expect::Version(v)), None)), ABSENT);
        assert_eq!(conflict(check(Some(&Expect::Absent), Some(&w))), w);
    }
}
