//! Mutes: "stop bringing up X" without deleting anything (card #1485, pack
//! D118, D124).
//!
//! A mute is a preference memory in category [`CATEGORY`] whose
//! `metadata.mute` names one target: an entity, a topic (and every topic
//! below it) or a tag. While it is `on`, the briefs, both hooks, the digest's
//! recent list and dead-end warnings in briefs leave out every memory it
//! covers. An explicit search still returns them, marked `muted`.
//!
//! Mutes are memories so they can be set from any client with the core
//! tools: `remind_me_add` mutes, `remind_me_update` with `on: false` unmutes
//! (its revision is the history), `remind_me_search` with `category: mute`
//! lists them. [`mute`], [`unmute`] and [`list`] are the CLI's wrappers.
//!
//! A mute is global unless it names a `project`; then it applies only to a
//! brief or hook in that project. A brief whose project is a muted entity
//! ignores that one mute, since naming it is an explicit ask.

use crate::db::memories::Memories;
use crate::db::{Result, Store, StoreError};
use crate::entity::{normalize_entity_name, resolve_entity};
use crate::kinds::{MemoryKind, StructuredFields};
use crate::models::{Memory, MemoryAddInput, MemoryUpdateInput, UpdateOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

/// The category every mute record is filed under.
pub const CATEGORY: &str = "mute";
/// The metadata key holding a mute's [`Spec`].
pub const KEY: &str = "mute";
/// The kind every mute record has.
pub const KIND: MemoryKind = MemoryKind::Preference;

/// What a mute names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Entity,
    Topic,
    Tag,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Topic => "topic",
            Self::Tag => "tag",
        }
    }

    /// Parse a target name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for anything but `entity`, `topic` or `tag`.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "entity" => Ok(Self::Entity),
            "topic" => Ok(Self::Topic),
            "tag" => Ok(Self::Tag),
            other => Err(StoreError::Invalid(format!(
                "mute target {other:?} must be entity, topic or tag"
            ))),
        }
    }
}

/// A mute's `metadata.mute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub target: Target,
    pub value: String,
    pub on: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// One mute record, as [`list`] returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mute {
    pub id: String,
    #[serde(flatten)]
    pub spec: Spec,
    pub updated_at: String,
}

/// Whether `memory` is a mute record.
pub fn is_record(memory: &Memory) -> bool {
    memory.category == CATEGORY
}

/// The [`Spec`] in `metadata`, strictly.
///
/// # Errors
///
/// [`StoreError::Invalid`] when `metadata.mute` is missing, malformed or
/// names a blank value.
pub fn spec_of(metadata: &Value) -> Result<Spec> {
    let raw = metadata.get(KEY).cloned().ok_or_else(|| {
        StoreError::Invalid(
            "a mute needs metadata.mute = {target: entity|topic|tag, value, on, project?}".into(),
        )
    })?;
    let mut spec: Spec = serde_json::from_value(raw)
        .map_err(|e| StoreError::Invalid(format!("metadata.mute: {e}")))?;
    spec.value = spec.value.trim().to_string();
    spec.project = spec
        .project
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    if spec.value.is_empty() {
        return Err(StoreError::Invalid("metadata.mute.value is empty".into()));
    }
    Ok(spec)
}

/// Check a write that files a memory under [`CATEGORY`]: it must be a
/// preference with a valid spec whose entity or topic exists. Other
/// categories pass untouched.
///
/// # Errors
///
/// [`StoreError::Invalid`] naming what is wrong.
pub fn check_write(
    store: &Store<'_>,
    category: &str,
    kind: Option<&str>,
    metadata: &Value,
) -> Result<()> {
    if category != CATEGORY {
        return Ok(());
    }
    if kind != Some(KIND.as_str()) {
        return Err(StoreError::Invalid(format!(
            "a mute is memory_type {}",
            KIND.as_str()
        )));
    }
    let spec = spec_of(metadata)?;
    match spec.target {
        Target::Tag => Ok(()),
        Target::Entity => check_entity(store, &spec.value),
        Target::Topic => check_topic(store, &spec.value).map(|_| ()),
    }
}

fn check_entity(store: &Store<'_>, name: &str) -> Result<()> {
    if name.to_ascii_lowercase().starts_with("topic:") {
        return Err(StoreError::Invalid(format!(
            "{name:?} is a topic: mute it with target topic"
        )));
    }
    match resolve_entity(store, name)? {
        Some(_) => Ok(()),
        None => Err(StoreError::Invalid(format!(
            "no entity named {name:?}; find its name with remind_me_entity"
        ))),
    }
}

/// The ids of the memories in `name`'s topic subtree.
fn check_topic(store: &Store<'_>, name: &str) -> Result<HashSet<String>> {
    let parsed = crate::topic::TopicName::parse(name)
        .map_err(|e| StoreError::Invalid(format!("mute topic {name:?}: {e}")))?;
    let tree = crate::topic::tree(store)?;
    let subtree = tree.subtree(&parsed);
    if subtree.is_empty() {
        return Err(StoreError::Invalid(format!("no topic named {name:?}")));
    }
    let memories = Memories::new(store);
    let mut members = HashSet::new();
    for topic in subtree {
        members.extend(memories.linked_ids(&topic.entity_id)?);
    }
    Ok(members)
}

/// Every mute record, on or off, newest change first.
///
/// # Errors
///
/// A store failure.
pub fn list(store: &Store<'_>) -> Result<Vec<Mute>> {
    let mut mutes: Vec<Mute> = records(store)?
        .into_iter()
        .filter_map(|m| {
            spec_of(&m.metadata).ok().map(|spec| Mute {
                id: m.id,
                spec,
                updated_at: m.updated_at,
            })
        })
        .collect();
    mutes.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(mutes)
}

/// The ids of every mute record, for writers that must leave them alone
/// (consolidation).
///
/// # Errors
///
/// A store failure.
pub fn record_ids(store: &Store<'_>) -> Result<HashSet<String>> {
    Ok(records(store)?.into_iter().map(|m| m.id).collect())
}

fn records(store: &Store<'_>) -> Result<Vec<Memory>> {
    Ok(Memories::new(store)
        .live_of_type(KIND.as_str())?
        .into_iter()
        .filter(|m| is_record(m) && m.superseded_by.is_none() && m.deleted_at.is_none())
        .collect())
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// `name` as the entity graph spells it, so an alias and its entity name
/// one mute and one project; a name no entity knows stays as written.
fn canonical(store: &Store<'_>, name: &str) -> Result<String> {
    let name = name.trim();
    Ok(resolve_entity(store, name)?.map_or_else(|| name.to_string(), |e| e.name))
}

/// A mute's value in its canonical spelling: entities by their graph name,
/// topics and tags as written.
fn canonical_value(store: &Store<'_>, target: Target, value: &str) -> Result<String> {
    match target {
        Target::Entity => canonical(store, value),
        Target::Topic | Target::Tag => Ok(value.trim().to_string()),
    }
}

/// A project name in its canonical spelling, `None` when blank.
fn canonical_project(store: &Store<'_>, project: Option<&str>) -> Result<Option<String>> {
    project
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| canonical(store, p))
        .transpose()
}

/// The record for `target` and `value` in `project`, on or off, matched by
/// canonical spelling on both sides; the newest when several match.
fn find(
    store: &Store<'_>,
    target: Target,
    value: &str,
    project: Option<&str>,
) -> Result<Option<Mute>> {
    Ok(matching(store, target, value, project)?.into_iter().next())
}

/// Every record for `target` and `value` in `project`, newest first. More
/// than one matches once an entity merge folds two muted names into one.
fn matching(
    store: &Store<'_>,
    target: Target,
    value: &str,
    project: Option<&str>,
) -> Result<Vec<Mute>> {
    let value = canonical_value(store, target, value)?;
    let project = canonical_project(store, project)?;
    let mut found = Vec::new();
    for mute in list(store)? {
        if mute.spec.target != target
            || !same(&canonical_value(store, target, &mute.spec.value)?, &value)
        {
            continue;
        }
        let stored = canonical_project(store, mute.spec.project.as_deref())?;
        let matches = match (stored.as_deref(), project.as_deref()) {
            (None, None) => true,
            (Some(a), Some(b)) => same(a, b),
            _ => false,
        };
        if matches {
            found.push(mute);
        }
    }
    Ok(found)
}

/// Mute `value`: turn its record back on, or write a new one.
///
/// # Errors
///
/// [`StoreError::Invalid`] for an unknown entity or topic; a store failure.
pub fn mute(store: &Store<'_>, target: Target, value: &str, project: Option<&str>) -> Result<Mute> {
    let spec = Spec {
        target,
        value: canonical_value(store, target, value)?,
        on: true,
        project: canonical_project(store, project)?,
    };
    let metadata = json!({ KEY: spec });
    if let Some(existing) = find(store, target, value, spec.project.as_deref())? {
        return set_on(store, &existing, true);
    }
    let fields = StructuredFields {
        memory_type: Some(KIND.as_str().to_string()),
        ..StructuredFields::default()
    };
    let memory = crate::db::queries::add_memory_with(
        store,
        MemoryAddInput {
            content: format!(
                "Mute {} {}: stop bringing it up unasked",
                target.as_str(),
                spec.value
            ),
            category: CATEGORY.to_string(),
            source: "manual".to_string(),
            metadata,
            ..Default::default()
        },
        &fields,
    )?;
    Ok(Mute {
        id: memory.id,
        spec,
        updated_at: memory.updated_at,
    })
}

/// Unmute `value`: turn off every record that mutes it, keeping them and
/// their history, and return the newest. All of them, because an entity
/// merge can leave two records naming one entity. `None` when nothing
/// mutes it.
///
/// # Errors
///
/// A store failure, which writes none of the records.
pub fn unmute(
    store: &Store<'_>,
    target: Target,
    value: &str,
    project: Option<&str>,
) -> Result<Option<Mute>> {
    store.atomically(|store| {
        let mut newest = None;
        for existing in matching(store, target, value, project)? {
            let off = set_on(store, &existing, false)?;
            newest.get_or_insert(off);
        }
        Ok(newest)
    })
}

fn set_on(store: &Store<'_>, existing: &Mute, on: bool) -> Result<Mute> {
    if existing.spec.on == on {
        return Ok(existing.clone());
    }
    let spec = Spec {
        on,
        ..existing.spec.clone()
    };
    let input = MemoryUpdateInput {
        memory_id: existing.id.clone(),
        content: None,
        category: None,
        tags: None,
        metadata: Some(json!({ KEY: spec })),
        sensitive: None,
        clear_superseded: false,
        project: None,
        if_version: None,
    };
    match crate::db::queries::update_memory(store, &input)? {
        UpdateOutcome::Updated(memory) => Ok(Mute {
            id: memory.id.clone(),
            spec,
            updated_at: memory.updated_at.clone(),
        }),
        UpdateOutcome::NotFound | UpdateOutcome::NoFields => Err(StoreError::NotFound),
    }
}

/// The memories the mutes in force cover, read once per brief or search.
#[derive(Debug, Clone, Default)]
pub struct MuteSet {
    /// Normalized entity names and aliases, for subject and object.
    entity_names: HashSet<String>,
    /// Memories linked to a muted entity or in a muted topic subtree.
    linked: HashSet<String>,
    /// Lowercased tags.
    tags: HashSet<String>,
}

impl MuteSet {
    /// The mutes in force for a request in `project` (`None`: global ones
    /// only), less an entity mute on `except_entity` (the brief's own
    /// project entity). A mute whose entity or topic no longer exists covers
    /// nothing.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub fn load(
        store: &Store<'_>,
        project: Option<&str>,
        except_entity: Option<&str>,
    ) -> Result<Self> {
        let mut set = Self::default();
        let except = except_entity.map(normalize_entity_name);
        let project = canonical_project(store, project)?;
        for mute in list(store)?.into_iter().filter(|m| m.spec.on) {
            let want = canonical_project(store, mute.spec.project.as_deref())?;
            let applies = match (want.as_deref(), project.as_deref()) {
                (None, _) => true,
                (Some(want), Some(have)) => same(want, have),
                (Some(_), None) => false,
            };
            if applies {
                set.add(store, &mute.spec, except.as_deref())?;
            }
        }
        Ok(set)
    }

    fn add(&mut self, store: &Store<'_>, spec: &Spec, except: Option<&str>) -> Result<()> {
        match spec.target {
            Target::Tag => {
                self.tags.insert(spec.value.to_lowercase());
            }
            Target::Entity => {
                let Some(entity) = resolve_entity(store, &spec.value)? else {
                    return Ok(());
                };
                let name = normalize_entity_name(&entity.name);
                if except == Some(name.as_str()) {
                    return Ok(());
                }
                self.linked
                    .extend(Memories::new(store).linked_ids(&entity.id)?);
                // Aliases too: a merge (card #567) moves the absorbed name
                // here, and facts naming it in subject or object stay muted.
                self.entity_names.extend(
                    std::iter::once(name)
                        .chain(entity.aliases.iter().map(|a| normalize_entity_name(a))),
                );
            }
            Target::Topic => {
                if let Ok(members) = check_topic(store, &spec.value) {
                    self.linked.extend(members);
                }
            }
        }
        Ok(())
    }

    /// Whether no mute is in force.
    pub fn is_empty(&self) -> bool {
        self.entity_names.is_empty() && self.linked.is_empty() && self.tags.is_empty()
    }

    /// Whether a mute covers `memory`. Mute records themselves are never
    /// covered.
    pub fn covers(&self, memory: &Memory) -> bool {
        if self.is_empty() || is_record(memory) {
            return false;
        }
        let names = |v: &Option<String>| {
            v.as_deref()
                .is_some_and(|v| self.entity_names.contains(&normalize_entity_name(v)))
        };
        self.linked.contains(&memory.id)
            || names(&memory.subject)
            || names(&memory.object)
            || memory
                .tags
                .iter()
                .any(|t| self.tags.contains(&t.to_lowercase()))
    }
}

/// Mark each hit of a search response's JSON whose id is in `muted` with
/// `"muted": true`, compact (`memories[i].id`) or full
/// (`memories[i].memory.id`).
pub fn mark_hits(value: &mut Value, muted: &HashSet<String>) {
    if muted.is_empty() {
        return;
    }
    let Some(hits) = value.get_mut("memories").and_then(Value::as_array_mut) else {
        return;
    };
    for hit in hits {
        let id = hit
            .get("id")
            .or_else(|| hit.get("memory").and_then(|m| m.get("id")))
            .and_then(Value::as_str)
            .map(str::to_string);
        if id.is_some_and(|id| muted.contains(&id)) {
            hit["muted"] = json!(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_is_parsed_strictly() {
        let ok = spec_of(&json!({ "mute": { "target": "tag", "value": " Jira ", "on": true } }))
            .unwrap();
        assert_eq!(ok.value, "Jira");
        assert_eq!(ok.target, Target::Tag);
        for bad in [
            json!({}),
            json!({ "mute": { "target": "colour", "value": "x", "on": true } }),
            json!({ "mute": { "target": "tag", "value": " ", "on": true } }),
            json!({ "mute": { "target": "tag", "value": "x" } }),
            json!({ "mute": { "target": "tag", "value": "x", "on": true, "extra": 1 } }),
        ] {
            assert!(spec_of(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn targets_parse_by_name() {
        assert_eq!(Target::parse("Entity").unwrap(), Target::Entity);
        assert!(Target::parse("project").is_err());
    }

    #[test]
    fn hits_are_marked_in_either_shape() {
        let muted: HashSet<String> = ["m1".to_string()].into();
        let mut compact = json!({ "memories": [{ "id": "m1" }, { "id": "m2" }] });
        mark_hits(&mut compact, &muted);
        assert_eq!(compact["memories"][0]["muted"], true);
        assert!(compact["memories"][1].get("muted").is_none());
        let mut full = json!({ "memories": [{ "memory": { "id": "m1" } }] });
        mark_hits(&mut full, &muted);
        assert_eq!(full["memories"][0]["muted"], true);
    }
}
