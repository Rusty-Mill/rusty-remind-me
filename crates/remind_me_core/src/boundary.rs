//! The write boundary's shared passes: redact, then index.
//!
//! Every path that stores new text (`add_memory`, both capture halves,
//! decompose, the importers) calls [`scrub`] before the insert and
//! [`index`] after it, so the rules in `redact.rs` and `extract.rs` apply
//! the same everywhere without each caller restating them.

use crate::attachments::ResolvedAttachment;
use crate::db::references::{NewReference, References};
use crate::db::{Result, Store};
use crate::entity::apply_entity_mentions;
use crate::extract;
use crate::models::EntityInput;
use serde_json::Value;
use std::collections::HashSet;

/// Redact secrets in `content` (tag and metadata updated in place); see
/// [`crate::redact::guard`].
pub fn scrub(content: &str, tags: &mut Vec<String>, metadata: &mut Value) -> String {
    crate::redact::guard(content, tags, metadata)
}

/// Redact secrets in a field kept beside the content, such as a title,
/// with no tags or metadata of its own to mark: the content's own [`scrub`]
/// marks the memory. Redact the whole source before it is cut, or a secret
/// split by the cut keeps a prefix no rule matches.
pub fn scrub_field(text: &str) -> String {
    if !crate::redact::enabled_by_env() {
        return text.to_string();
    }
    crate::redact::redact(text).content
}

/// Redact secrets in raw JSONL before it is kept verbatim (the session
/// archive). Each string inside a line is redacted as [`scrub`] would
/// redact it, so a line stays valid JSON; a line that does not parse is
/// redacted as text. Lines with nothing to redact are kept byte for byte.
pub fn scrub_jsonl(raw: &str) -> String {
    if !crate::redact::enabled_by_env() {
        return raw.to_string();
    }
    raw.split_inclusive('\n').map(scrub_line).collect()
}

fn scrub_line(line: &str) -> String {
    let body = line.trim_end_matches(['\n', '\r']);
    let end = &line[body.len()..];
    let Ok(mut value) = serde_json::from_str::<Value>(body) else {
        return format!("{}{end}", crate::redact::redact(body).content);
    };
    if !redact_strings(&mut value) {
        return line.to_string();
    }
    format!("{value}{end}")
}

/// Redact every string in `value` in place; true when anything fired.
fn redact_strings(value: &mut Value) -> bool {
    match value {
        Value::String(text) => {
            let redacted = crate::redact::redact(text);
            let hit = !redacted.hits.is_empty();
            if hit {
                *text = redacted.content;
            }
            hit
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |hit, v| redact_strings(v) | hit),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |hit, v| redact_strings(v) | hit),
        _ => false,
    }
}

/// Caller entities first, so the caller's kind wins; extracted names that
/// match one of theirs (case-insensitively) are dropped.
fn merge_entities(caller: &[EntityInput], found: Vec<EntityInput>) -> Vec<EntityInput> {
    let mut seen: HashSet<String> = caller
        .iter()
        .map(|e| e.name.trim().to_lowercase())
        .collect();
    let mut all = caller.to_vec();
    all.extend(
        found
            .into_iter()
            .filter(|e| seen.insert(e.name.to_lowercase())),
    );
    all
}

/// After the insert: write extracted references, attachment references and
/// the merged entity mentions. Returns how many entities were linked.
pub fn index(
    store: &Store<'_>,
    memory_id: &str,
    content: &str,
    caller_entities: &[EntityInput],
    extract_on: bool,
    attachments: &[ResolvedAttachment],
    now: &str,
) -> Result<usize> {
    let refs = References::new(store);
    let found = if extract_on && extract::enabled_by_env() {
        extract::extract(content)
    } else {
        extract::Extracted::default()
    };
    for parts in &found.references {
        refs.insert(&NewReference {
            label: parts.label.clone(),
            ..NewReference::new(memory_id, parts.kind, parts.value.clone(), now)
        })?;
    }
    for a in attachments {
        refs.insert(&NewReference {
            label: Some(a.label.clone()),
            ..NewReference::new(memory_id, "attachment", a.value.clone(), now)
        })?;
    }
    let entities = merge_entities(caller_entities, found.entities);
    apply_entity_mentions(store, memory_id, &entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, kind: Option<&str>) -> EntityInput {
        EntityInput {
            name: name.into(),
            kind: kind.map(str::to_string),
            aliases: vec![],
        }
    }

    #[test]
    fn caller_entities_win_over_extracted_ones() {
        let merged = merge_entities(
            &[e("Rusty Mill", Some("project"))],
            vec![e("rusty mill", None), e("Daily Backup", None)],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].kind.as_deref(), Some("project"));
        assert_eq!(merged[1].name, "Daily Backup");
    }

    fn token() -> String {
        format!("ghp_{}", "a".repeat(36))
    }

    #[test]
    fn scrub_jsonl_redacts_inside_strings_and_keeps_lines_valid() {
        let secret =
            serde_json::json!({"message": {"content": [{"text": format!("key {}", token())}]}});
        let raw = format!("{secret}\n{{\"ok\": \"clean\"}}\nnot json {}\n", token());

        let kept = scrub_jsonl(&raw);

        assert!(!kept.contains(&token()), "{kept}");
        let lines: Vec<&str> = kept.lines().collect();
        assert_eq!(lines.len(), 3);
        let first: Value = serde_json::from_str(lines[0]).expect("still JSON");
        assert!(first.to_string().contains("[REDACTED:github_token]"));
        assert_eq!(
            lines[1], "{\"ok\": \"clean\"}",
            "a clean line is kept byte for byte"
        );
        assert!(lines[2].starts_with("not json [REDACTED:"));
        assert!(kept.ends_with('\n'));
    }
}
