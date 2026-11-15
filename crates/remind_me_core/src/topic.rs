//! The topic taxonomy's core (design: `docs/design/topic-taxonomy.md`).
//!
//! A topic is an entity named `topic:<Label>`. The prefix keeps topic
//! membership apart from mention links: a memory that *mentions* MBSE links
//! to the entity `mbse`, a memory *about* MBSE links to `topic:MBSE`. The
//! generic write paths refuse `topic:` entity names ([`refuse_topic_name`]),
//! so this module is the only writer of membership.
//!
//! The tree lives in claims, as projects do (ADR-0027): append-only fact
//! memories with predicates `topic.parent` and `topic.status`, each citing
//! evidence. A move is a superseding claim, so the history keeps every
//! earlier parent. Relations were not used because they are immutable: a
//! topic could never move.
//!
//! [`TopicTree`] is a projection built from live claims. A topic whose claims
//! are malformed, whose parent is not a valid topic, or that sits in a cycle
//! is reported in [`TopicTree::invalid`], never dropped.

use crate::db::entities::Entities;
use crate::db::memories::{Memories, NewMemory};
use crate::db::{Result, Store, StoreError};
use crate::entity::{entity_id, normalize_entity_name, upsert_entity};
use crate::models::{EntityInput, Memory};
use crate::project::{check_live, fact_from, looks_secret, Evidence, InvalidRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::str::FromStr;

/// Every topic entity's name starts with this.
pub const ENTITY_PREFIX: &str = "topic:";
/// Every topic claim predicate starts with this.
pub const PREDICATE_PREFIX: &str = "topic.";
/// Tag on every topic claim memory.
pub const CLAIM_TAG: &str = "topic-record";
/// Tag on a `because` sentence saved as evidence.
pub const EVIDENCE_TAG: &str = "topic-evidence";
/// The parent value of a top-level topic.
pub const ROOT: &str = "root";
/// The longest label a topic may have, in characters.
pub const MAX_LABEL_CHARS: usize = 80;
/// `source` of every memory this module writes. Search leaves these out.
pub const SOURCE: &str = "topic";
/// Key of the claim's block inside a memory's metadata.
const META_KEY: &str = "topic_claim";

// --- names -----------------------------------------------------------------

/// A topic's entity name, always `topic:<Label>` with the label trimmed and
/// its whitespace collapsed. Built only by [`TopicName::parse`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TopicName(String);

impl TopicName {
    /// Parse `MBSE` or `topic:MBSE` (prefix case-insensitive). Refuses an
    /// empty label, one over [`MAX_LABEL_CHARS`], the reserved `root`, and a
    /// label that looks like a secret.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let label = strip_prefix(trimmed).unwrap_or(trimmed);
        let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
        if label.is_empty() {
            return Err(StoreError::Invalid("topic label is empty".into()));
        }
        if label.chars().count() > MAX_LABEL_CHARS {
            return Err(StoreError::Invalid(format!(
                "topic label is longer than {MAX_LABEL_CHARS} characters"
            )));
        }
        if label.eq_ignore_ascii_case(ROOT) {
            return Err(StoreError::Invalid(format!(
                "'{ROOT}' is reserved for the top of the tree"
            )));
        }
        if looks_secret(&label) {
            return Err(StoreError::Invalid(
                "topic label looks like a secret".into(),
            ));
        }
        Ok(Self(format!("{ENTITY_PREFIX}{label}")))
    }

    /// The full entity name, `topic:<Label>`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The label without the prefix.
    pub fn label(&self) -> &str {
        &self.0[ENTITY_PREFIX.len()..]
    }

    /// The topic's entity id.
    pub fn entity_id(&self) -> String {
        entity_id(&self.0)
    }

    /// The identity two spellings share: the normalised entity name.
    fn key(&self) -> String {
        normalize_entity_name(&self.0)
    }
}

impl fmt::Display for TopicName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn strip_prefix(name: &str) -> Option<&str> {
    let head = name.get(..ENTITY_PREFIX.len())?;
    head.eq_ignore_ascii_case(ENTITY_PREFIX)
        .then(|| &name[ENTITY_PREFIX.len()..])
}

/// Whether `name` is a topic entity name.
pub fn is_topic_name(name: &str) -> bool {
    strip_prefix(name.trim()).is_some()
}

/// Refuse a generic entity write that names a topic: membership is written
/// only through [`assign`].
pub fn refuse_topic_name(name: &str) -> Result<()> {
    if is_topic_name(name) {
        return Err(StoreError::Invalid(format!(
            "entity name '{}' is reserved for topics; topics are assigned by the topics commands",
            name.trim()
        )));
    }
    Ok(())
}

/// [`refuse_topic_name`] for every entity in a write.
pub fn refuse_topic_entities(entities: &[EntityInput]) -> Result<()> {
    entities.iter().try_for_each(|e| refuse_topic_name(&e.name))
}

// --- claim values ----------------------------------------------------------

/// Where a topic is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Suggested (by a rebalance), not yet confirmed; takes no members.
    Proposed,
    /// In use: takes members, shows in the tree.
    Active,
    /// Merged away or abandoned; kept for history, takes no members.
    Retired,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

impl FromStr for Status {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "proposed" => Ok(Self::Proposed),
            "active" => Ok(Self::Active),
            "retired" => Ok(Self::Retired),
            other => Err(StoreError::Invalid(format!(
                "unknown topic status '{other}' (expected one of: proposed, active, retired)"
            ))),
        }
    }
}

/// A topic's parent: the top of the tree, or another topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Parent {
    #[serde(serialize_with = "serialize_root")]
    Root,
    Topic(TopicName),
}

fn serialize_root<S: serde::Serializer>(s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(ROOT)
}

impl Parent {
    /// `root`, or a topic name.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.trim().eq_ignore_ascii_case(ROOT) {
            return Ok(Self::Root);
        }
        TopicName::parse(raw).map(Self::Topic)
    }

    fn object(&self) -> String {
        match self {
            Self::Root => ROOT.to_string(),
            Self::Topic(name) => name.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Predicate {
    Parent,
    Status,
}

impl Predicate {
    fn qualified(self) -> &'static str {
        match self {
            Self::Parent => "topic.parent",
            Self::Status => "topic.status",
        }
    }

    fn from_qualified(predicate: &str) -> Option<Self> {
        match normalize_entity_name(predicate).as_str() {
            "topic.parent" => Some(Self::Parent),
            "topic.status" => Some(Self::Status),
            _ => None,
        }
    }
}

/// What a claim memory's metadata records under `topic_claim`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaimMeta {
    evidence: Vec<String>,
    confirmed_by: String,
}

// --- the projection --------------------------------------------------------

/// One topic, as its live claims describe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Topic {
    pub name: TopicName,
    pub entity_id: String,
    pub parent: Parent,
    pub status: Status,
    /// Live memories assigned directly to this topic (not its descendants).
    pub member_count: usize,
    pub parent_claim_id: String,
    pub status_claim_id: String,
}

/// Every valid topic, and every topic the projection could not build.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TopicTree {
    /// Valid topics, ordered by normalised name.
    pub topics: Vec<Topic>,
    pub invalid: Vec<InvalidRecord>,
}

impl TopicTree {
    /// The topic named `name` (any spelling of it), if valid.
    pub fn get(&self, name: &TopicName) -> Option<&Topic> {
        let key = name.key();
        self.topics.iter().find(|t| t.name.key() == key)
    }

    /// Topics directly under `parent`.
    pub fn children(&self, parent: &Parent) -> impl Iterator<Item = &Topic> + '_ {
        let parent = parent.clone();
        self.topics
            .iter()
            .filter(move |t| same_parent(&t.parent, &parent))
    }

    /// `name` and every topic below it, `name` first. Empty when `name` is
    /// not a valid topic.
    pub fn subtree(&self, name: &TopicName) -> Vec<&Topic> {
        let Some(top) = self.get(name) else {
            return Vec::new();
        };
        let mut out = vec![top];
        let mut next = 0;
        while next < out.len() {
            let parent = Parent::Topic(out[next].name.clone());
            out.extend(self.children(&parent));
            next += 1;
        }
        out
    }
}

fn same_parent(a: &Parent, b: &Parent) -> bool {
    match (a, b) {
        (Parent::Root, Parent::Root) => true,
        (Parent::Topic(x), Parent::Topic(y)) => x.key() == y.key(),
        _ => false,
    }
}

/// Live claims of one topic, by predicate.
struct Live<'m> {
    subject: String,
    parent: Option<&'m Memory>,
    status: Option<&'m Memory>,
}

/// A topic whose own claims parse, before its parent is checked.
struct Draft {
    topic: Topic,
}

/// Build the tree from every live topic claim.
pub fn tree(store: &Store<'_>) -> Result<TopicTree> {
    let claims = Memories::new(store).with_predicate_prefix(PREDICATE_PREFIX)?;
    let mut out = TopicTree::default();
    let mut groups: BTreeMap<String, Live<'_>> = BTreeMap::new();
    for memory in claims.iter().filter(|m| m.superseded_by.is_none()) {
        let subject = memory.subject.as_deref().unwrap_or_default();
        let Ok(name) = TopicName::parse(subject).map_err(|e| {
            out.invalid.push(invalid(
                subject,
                Some(&memory.id),
                format!("bad subject: {e}"),
            ))
        }) else {
            continue;
        };
        let live = groups.entry(name.key()).or_insert_with(|| Live {
            subject: name.as_str().to_string(),
            parent: None,
            status: None,
        });
        // `claims` is oldest first, so the newest live claim wins.
        match memory
            .predicate
            .as_deref()
            .and_then(Predicate::from_qualified)
        {
            Some(Predicate::Parent) => live.parent = Some(memory),
            Some(Predicate::Status) => live.status = Some(memory),
            None => out.invalid.push(invalid(
                subject,
                Some(&memory.id),
                format!(
                    "unknown predicate '{}'",
                    memory.predicate.as_deref().unwrap_or("")
                ),
            )),
        }
    }

    let entities = Entities::new(store);
    let mut drafts: BTreeMap<String, Draft> = BTreeMap::new();
    for (key, live) in groups {
        match draft_from(&live, &entities)? {
            Ok(draft) => {
                drafts.insert(key, draft);
            }
            Err(record) => out.invalid.push(record),
        }
    }
    let valid = valid_keys(&drafts, &mut out.invalid);
    out.topics = drafts
        .into_iter()
        .filter(|(key, _)| valid.get(key) == Some(&true))
        .map(|(_, d)| d.topic)
        .collect();
    Ok(out)
}

fn invalid(subject: &str, claim_id: Option<&str>, reason: String) -> InvalidRecord {
    InvalidRecord {
        subject: subject.to_string(),
        claim_id: claim_id.map(str::to_string),
        reason,
    }
}

/// A topic from its live claims, or why it cannot be one.
fn draft_from(
    live: &Live<'_>,
    entities: &Entities<'_>,
) -> Result<std::result::Result<Draft, InvalidRecord>> {
    let (Some(parent_m), Some(status_m)) = (live.parent, live.status) else {
        return Ok(Err(invalid(
            &live.subject,
            None,
            "needs both a live topic.parent and topic.status claim".into(),
        )));
    };
    let checked = check_claim(parent_m)
        .and_then(|()| check_claim(status_m))
        .and_then(|()| Parent::parse(object(parent_m)).map_err(|e| e.to_string()))
        .and_then(|p| {
            Status::from_str(object(status_m))
                .map(|s| (p, s))
                .map_err(|e| e.to_string())
        });
    let (parent, status) = match checked {
        Ok(pair) => pair,
        Err(reason) => return Ok(Err(invalid(&live.subject, None, reason))),
    };
    // The subject parsed when it was grouped.
    let name = TopicName::parse(&live.subject)?;
    let entity_id = name.entity_id();
    Ok(Ok(Draft {
        topic: Topic {
            member_count: entities.linked_memory_count(&entity_id)?,
            entity_id,
            name,
            parent,
            status,
            parent_claim_id: parent_m.id.clone(),
            status_claim_id: status_m.id.clone(),
        },
    }))
}

fn object(memory: &Memory) -> &str {
    memory.object.as_deref().unwrap_or_default()
}

/// A claim's metadata names at least one evidence id and who confirmed it.
fn check_claim(memory: &Memory) -> std::result::Result<(), String> {
    let block = memory
        .metadata
        .get(META_KEY)
        .ok_or_else(|| format!("claim {} has no '{META_KEY}' block", memory.id))?;
    let meta: ClaimMeta = serde_json::from_value(block.clone())
        .map_err(|e| format!("claim {} has a bad '{META_KEY}': {e}", memory.id))?;
    Evidence::new(meta.evidence)
        .map(|_| ())
        .ok_or_else(|| format!("claim {} cites no evidence", memory.id))
}

/// Which drafts reach the root through valid parents. A draft whose parent
/// is missing, invalid or in a cycle is reported.
fn valid_keys(
    drafts: &BTreeMap<String, Draft>,
    invalid_out: &mut Vec<InvalidRecord>,
) -> HashMap<String, bool> {
    let mut valid: HashMap<String, bool> = HashMap::new();
    for start in drafts.keys() {
        let mut path: Vec<&str> = Vec::new();
        let mut at = start.as_str();
        let verdict = loop {
            if let Some(&known) = valid.get(at) {
                break known;
            }
            if path.contains(&at) {
                break false;
            }
            path.push(at);
            match drafts.get(at).map(|d| &d.topic.parent) {
                None => break false,
                Some(Parent::Root) => break true,
                Some(Parent::Topic(parent)) => at = drafts_key(drafts, parent),
            }
        };
        for key in path {
            valid.entry(key.to_string()).or_insert(verdict);
        }
    }
    for (key, draft) in drafts {
        if valid.get(key) == Some(&false) {
            let parent = match &draft.topic.parent {
                Parent::Topic(p) => p.as_str().to_string(),
                Parent::Root => ROOT.to_string(),
            };
            invalid_out.push(invalid(
                draft.topic.name.as_str(),
                Some(&draft.topic.parent_claim_id),
                format!(
                    "parent '{parent}' is not a valid topic, or the parent chain forms a cycle"
                ),
            ));
        }
    }
    valid
}

/// `parent`'s key in `drafts`, or a key no draft has.
fn drafts_key<'a>(drafts: &'a BTreeMap<String, Draft>, parent: &TopicName) -> &'a str {
    drafts
        .get_key_value(&parent.key())
        .map_or("", |(k, _)| k.as_str())
}

// --- writes ----------------------------------------------------------------

/// A request to create a topic or change its parent or status.
#[derive(Debug, Clone)]
pub struct SetInput {
    pub topic: TopicName,
    pub parent: Option<Parent>,
    pub status: Option<Status>,
    /// Ids of live memories supporting the change.
    pub evidence: Vec<String>,
    /// A sentence saved as a fact memory and cited as evidence.
    pub because: Option<String>,
    pub confirmed_by: String,
}

/// What a [`set`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SetOutcome {
    pub topic: String,
    /// New claim memory ids.
    pub written: Vec<String>,
    /// Claim ids the new claims superseded.
    pub superseded: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub because_id: Option<String>,
}

/// Create a topic (give both parent and status) or change an existing one.
///
/// Refused, with nothing written: a blank `confirmed_by`; no evidence and no
/// `because`; an evidence id that is not a live memory; a new topic without
/// both claims; a parent that is not a valid topic; a parent that would put
/// the topic under itself. A claim equal to the live value is skipped. All
/// writes happen in one transaction.
pub fn set(store: &Store<'_>, input: &SetInput) -> Result<SetOutcome> {
    store.transaction(|page| set_in(page, input))
}

fn set_in(store: &Store<'_>, input: &SetInput) -> Result<SetOutcome> {
    let confirmed_by = input.confirmed_by.trim();
    if confirmed_by.is_empty() {
        return Err(StoreError::Invalid("confirmed_by is required".into()));
    }
    if input.parent.is_none() && input.status.is_none() {
        return Err(StoreError::Invalid(
            "give a parent, a status or both".into(),
        ));
    }
    let because = input
        .because
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    if input.evidence.iter().all(|id| id.trim().is_empty()) && because.is_none() {
        return Err(StoreError::Invalid(
            "give evidence ids or a because sentence".into(),
        ));
    }
    check_live(store, &input.evidence)?;

    let current = tree(store)?;
    let existing = current.get(&input.topic);
    if existing.is_none() && (input.parent.is_none() || input.status.is_none()) {
        return Err(StoreError::Invalid(format!(
            "'{}' is not a topic yet: give both a parent and a status",
            input.topic
        )));
    }
    if let Some(Parent::Topic(parent)) = &input.parent {
        check_parent(&current, &input.topic, parent)?;
    }

    let claims = Memories::new(store).with_predicate_prefix(PREDICATE_PREFIX)?;
    let changes: Vec<(Predicate, String)> = [
        input
            .parent
            .as_ref()
            .map(|p| (Predicate::Parent, p.object()))
            .filter(|(_, o)| existing.is_none_or(|t| t.parent.object() != *o)),
        input
            .status
            .map(|s| (Predicate::Status, s.as_str().to_string()))
            .filter(|(_, o)| existing.is_none_or(|t| t.status.as_str() != o)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut outcome = SetOutcome {
        topic: input.topic.as_str().to_string(),
        ..SetOutcome::default()
    };
    if changes.is_empty() {
        return Ok(outcome);
    }

    upsert_entity(
        store,
        &EntityInput {
            name: input.topic.as_str().to_string(),
            kind: Some("topic".into()),
            aliases: Vec::new(),
        },
    )?;
    let mut ids = input.evidence.clone();
    if let Some(sentence) = because {
        let row = fact_from(
            SOURCE,
            sentence,
            vec![EVIDENCE_TAG.to_string()],
            Value::Object(Default::default()),
        );
        Memories::new(store).insert(&row)?;
        ids.push(row.id.clone());
        outcome.because_id = Some(row.id);
    }
    let evidence = Evidence::new(ids)
        .ok_or_else(|| StoreError::Invalid("give evidence ids or a because sentence".into()))?;
    let meta = ClaimMeta {
        evidence: evidence.ids().to_vec(),
        confirmed_by: confirmed_by.to_string(),
    };
    for (predicate, object) in changes {
        let (id, superseded) =
            write_claim(store, &input.topic, predicate, &object, &meta, &claims)?;
        outcome.written.push(id);
        outcome.superseded.extend(superseded);
    }
    Ok(outcome)
}

/// `parent` must be a valid topic that is not `topic` or below it.
fn check_parent(tree: &TopicTree, topic: &TopicName, parent: &TopicName) -> Result<()> {
    if tree.get(parent).is_none() {
        return Err(StoreError::Invalid(format!(
            "parent '{parent}' is not a valid topic"
        )));
    }
    let key = parent.key();
    if tree.subtree(topic).iter().any(|t| t.name.key() == key) || topic.key() == key {
        return Err(StoreError::Invalid(format!(
            "'{parent}' is '{topic}' or below it; a topic cannot sit under itself"
        )));
    }
    Ok(())
}

/// Write one claim and supersede the live claims of its predicate.
fn write_claim(
    store: &Store<'_>,
    topic: &TopicName,
    predicate: Predicate,
    object: &str,
    meta: &ClaimMeta,
    claims: &[Memory],
) -> Result<(String, Vec<String>)> {
    let row = claim_row(topic, predicate, object, meta);
    let memories = Memories::new(store);
    memories.insert(&row)?;
    let key = topic.key();
    let mut superseded = Vec::new();
    for old in claims
        .iter()
        .filter(|m| m.superseded_by.is_none())
        .filter(|m| m.subject.as_deref().map(normalize_entity_name) == Some(key.clone()))
        .filter(|m| m.predicate.as_deref().and_then(Predicate::from_qualified) == Some(predicate))
    {
        memories.set_superseded_by(&old.id, &row.id, Some(&row.created_at))?;
        superseded.push(old.id.clone());
    }
    Ok((row.id, superseded))
}

fn claim_row(topic: &TopicName, predicate: Predicate, object: &str, meta: &ClaimMeta) -> NewMemory {
    let short = predicate.qualified().trim_start_matches(PREDICATE_PREFIX);
    let mut row = fact_from(
        SOURCE,
        &format!("Topic {}: {short} is {object}", topic.label()),
        vec![CLAIM_TAG.to_string()],
        serde_json::json!({ META_KEY: meta }),
    );
    row.subject = Some(topic.as_str().to_string());
    row.predicate = Some(predicate.qualified().to_string());
    row.object = Some(object.to_string());
    row
}

// --- membership ------------------------------------------------------------

/// Put live memory `memory_id` under active topic `topic`. Whether the link
/// is new. A topic claim or evidence memory is refused: it describes the
/// tree, it is not knowledge in it.
pub fn assign(store: &Store<'_>, memory_id: &str, topic: &TopicName) -> Result<bool> {
    let current = tree(store)?;
    let found = current
        .get(topic)
        .ok_or_else(|| StoreError::Invalid(format!("'{topic}' is not a valid topic")))?;
    if found.status != Status::Active {
        return Err(StoreError::Invalid(format!(
            "'{topic}' is {}; only an active topic takes members",
            found.status.as_str()
        )));
    }
    let memory = Memories::new(store)
        .get_live(memory_id)?
        .ok_or(StoreError::NotFound)?;
    if memory.source == SOURCE {
        return Err(StoreError::Invalid(format!(
            "memory {memory_id} records the topic tree and cannot be a member"
        )));
    }
    crate::entity::link_memory_entity(store, memory_id, &found.entity_id)
}

/// Take `memory_id` out of `topic`. Whether it was a member.
pub fn unassign(store: &Store<'_>, memory_id: &str, topic: &TopicName) -> Result<bool> {
    Entities::new(store).unlink(memory_id, &topic.entity_id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::queries::get_memory_by_id;
    use crate::db::Database;

    fn name(s: &str) -> TopicName {
        TopicName::parse(s).unwrap()
    }

    fn because(text: &str) -> Option<String> {
        Some(text.to_string())
    }

    fn create(store: &Store<'_>, topic: &str, parent: &str) -> SetOutcome {
        set(
            store,
            &SetInput {
                topic: name(topic),
                parent: Some(Parent::parse(parent).unwrap()),
                status: Some(Status::Active),
                evidence: Vec::new(),
                because: because("seeded in a test"),
                confirmed_by: "human".into(),
            },
        )
        .unwrap()
    }

    fn move_to(store: &Store<'_>, topic: &str, parent: &str) -> Result<SetOutcome> {
        set(
            store,
            &SetInput {
                topic: name(topic),
                parent: Some(Parent::parse(parent).unwrap()),
                status: None,
                evidence: Vec::new(),
                because: because("moved in a test"),
                confirmed_by: "human".into(),
            },
        )
    }

    fn add_memory(store: &Store<'_>, content: &str) -> String {
        let row = fact_from(
            "manual",
            content,
            Vec::new(),
            Value::Object(Default::default()),
        );
        Memories::new(store).insert(&row).unwrap();
        row.id
    }

    #[test]
    fn names_carry_the_prefix_once_and_refuse_bad_labels() {
        assert_eq!(
            name("  Systems   Engineering ").as_str(),
            "topic:Systems Engineering"
        );
        assert_eq!(name("Topic:MBSE").as_str(), "topic:MBSE");
        assert_eq!(name("topic:MBSE").label(), "MBSE");
        assert_eq!(name("MBSE").entity_id(), name("topic:mbse").entity_id());
        assert!(TopicName::parse("topic:  ").is_err());
        assert!(TopicName::parse("root").is_err());
        assert!(TopicName::parse(&"x".repeat(MAX_LABEL_CHARS + 1)).is_err());
        assert!(TopicName::parse("ghp_abcdefghijklmnopqrstuvwxyz0123456789").is_err());
    }

    #[test]
    fn generic_paths_refuse_topic_names_and_predicates() {
        assert!(refuse_topic_name("topic:MBSE").is_err());
        assert!(refuse_topic_name(" TOPIC:mbse").is_err());
        assert!(refuse_topic_name("MBSE").is_ok());
        assert!(crate::project::refuse_reserved_predicate(Some("topic.parent")).is_err());
        assert!(crate::project::refuse_reserved_predicate(Some("topical")).is_ok());
    }

    #[test]
    fn a_tree_builds_from_claims_and_walks_down() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        create(&store, "Engineering", "root");
        create(&store, "Systems Engineering", "topic:Engineering");
        create(&store, "MBSE", "Systems Engineering");
        create(&store, "Cooking", "root");

        let tree = tree(&store).unwrap();
        assert!(tree.invalid.is_empty(), "{:?}", tree.invalid);
        assert_eq!(tree.topics.len(), 4);
        let roots: Vec<&str> = tree
            .children(&Parent::Root)
            .map(|t| t.name.label())
            .collect();
        assert_eq!(roots, ["Cooking", "Engineering"]);
        let below: Vec<&str> = tree
            .subtree(&name("engineering"))
            .iter()
            .map(|t| t.name.label())
            .collect();
        assert_eq!(below, ["Engineering", "Systems Engineering", "MBSE"]);
    }

    #[test]
    fn a_move_supersedes_the_old_parent_and_keeps_it() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        create(&store, "Engineering", "root");
        create(&store, "Software", "root");
        let first = tree(&store)
            .unwrap()
            .get(&name("Software"))
            .unwrap()
            .parent_claim_id
            .clone();

        let moved = move_to(&store, "Software", "Engineering").unwrap();
        assert_eq!(moved.superseded, std::slice::from_ref(&first));
        let tree = tree(&store).unwrap();
        assert_eq!(
            tree.get(&name("Software")).unwrap().parent,
            Parent::Topic(name("Engineering"))
        );
        let old = get_memory_by_id(&store, &first).unwrap().unwrap();
        assert_eq!(
            old.superseded_by.as_deref(),
            moved.written.first().map(String::as_str)
        );
    }

    #[test]
    fn an_unchanged_value_writes_nothing() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        create(&store, "Engineering", "root");
        let again = move_to(&store, "Engineering", "root").unwrap();
        assert!(again.written.is_empty());
        assert!(again.because_id.is_none());
    }

    #[test]
    fn writes_refuse_cycles_unknown_parents_and_half_topics() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        create(&store, "A", "root");
        create(&store, "B", "A");
        create(&store, "C", "B");

        assert!(
            move_to(&store, "A", "C").is_err(),
            "under its own grandchild"
        );
        assert!(move_to(&store, "A", "A").is_err(), "under itself");
        assert!(move_to(&store, "A", "Nowhere").is_err(), "unknown parent");
        let half = set(
            &store,
            &SetInput {
                topic: name("D"),
                parent: Some(Parent::Root),
                status: None,
                evidence: Vec::new(),
                because: because("x"),
                confirmed_by: "human".into(),
            },
        );
        assert!(half.is_err(), "a new topic needs a status too");
        let unevidenced = set(
            &store,
            &SetInput {
                topic: name("D"),
                parent: Some(Parent::Root),
                status: Some(Status::Active),
                evidence: Vec::new(),
                because: None,
                confirmed_by: "human".into(),
            },
        );
        assert!(unevidenced.is_err(), "no evidence");
        assert_eq!(tree(&store).unwrap().topics.len(), 3);
    }

    #[test]
    fn the_projection_reports_cycles_and_orphans_instead_of_dropping_them() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        // Claims a validated write would refuse, as a synced store could hold.
        let meta = ClaimMeta {
            evidence: vec!["mem_x".into()],
            confirmed_by: "sync".into(),
        };
        let memories = Memories::new(&store);
        for (topic, parent) in [("X", "topic:Y"), ("Y", "topic:X"), ("Z", "topic:Gone")] {
            for (p, o) in [(Predicate::Parent, parent), (Predicate::Status, "active")] {
                memories
                    .insert(&claim_row(&name(topic), p, o, &meta))
                    .unwrap();
            }
        }
        create(&store, "Fine", "root");

        let tree = tree(&store).unwrap();
        let labels: Vec<&str> = tree.topics.iter().map(|t| t.name.label()).collect();
        assert_eq!(labels, ["Fine"]);
        let mut bad: Vec<&str> = tree.invalid.iter().map(|r| r.subject.as_str()).collect();
        bad.sort_unstable();
        assert_eq!(bad, ["topic:X", "topic:Y", "topic:Z"]);
    }

    #[test]
    fn membership_needs_an_active_topic_and_a_knowledge_memory() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        create(&store, "MBSE", "root");
        let memory = add_memory(
            &store,
            "SysML v2 replaces block diagrams with a textual notation",
        );

        assert!(assign(&store, &memory, &name("MBSE")).unwrap());
        assert!(
            !assign(&store, &memory, &name("mbse")).unwrap(),
            "already a member"
        );
        assert_eq!(
            tree(&store)
                .unwrap()
                .get(&name("MBSE"))
                .unwrap()
                .member_count,
            1
        );

        assert!(assign(&store, &memory, &name("Nowhere")).is_err());
        let claim = tree(&store)
            .unwrap()
            .get(&name("MBSE"))
            .unwrap()
            .status_claim_id
            .clone();
        assert!(
            assign(&store, &claim, &name("MBSE")).is_err(),
            "a claim is not a member"
        );

        set(
            &store,
            &SetInput {
                topic: name("MBSE"),
                parent: None,
                status: Some(Status::Retired),
                evidence: vec![memory.clone()],
                because: None,
                confirmed_by: "human".into(),
            },
        )
        .unwrap();
        let other = add_memory(&store, "Capella is an MBSE tool");
        assert!(
            assign(&store, &other, &name("MBSE")).is_err(),
            "retired takes no members"
        );

        assert!(unassign(&store, &memory, &name("MBSE")).unwrap());
        assert!(!unassign(&store, &memory, &name("MBSE")).unwrap());
        assert_eq!(
            tree(&store)
                .unwrap()
                .get(&name("MBSE"))
                .unwrap()
                .member_count,
            0
        );
    }

    #[test]
    fn claims_cannot_be_edited_through_generic_paths() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        create(&store, "MBSE", "root");
        let id = tree(&store)
            .unwrap()
            .get(&name("MBSE"))
            .unwrap()
            .parent_claim_id
            .clone();
        let claim = get_memory_by_id(&store, &id).unwrap().unwrap();
        let refused = crate::project::refuse_claim_edit(&claim).unwrap_err();
        assert!(refused.to_string().contains("topic claim"), "{refused}");
    }
}
