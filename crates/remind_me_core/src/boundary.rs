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
}
