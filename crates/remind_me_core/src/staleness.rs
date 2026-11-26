//! Read-time stale flags on derived knowledge (card #60, D83, D87, D88).
//!
//! A wiki page, scenario or persona statement is written from some source
//! memories and then left alone. Nothing told a reader when newer evidence
//! about the same things had arrived since, so an outdated page read exactly
//! like a current one. [`stale_check`] answers that at read time, with no
//! schema change: the artifact is stale when a newer memory shares an entity
//! with its sources.
//!
//! # What counts as related (D87)
//!
//! Only a shared entity. Tags do not count, and neither does sharing a
//! project: in an active project nearly every page would read as stale within
//! a day. Project entities (kind `project`, or named like a source's project)
//! and hub entities linked to more than [`HUB_LINK_CAP`] memories are skipped
//! for the same reason, and because following a hub's links would break the
//! cost bound. The sources' project only narrows the scope: evidence written
//! in another project does not count.
//!
//! # What counts as evidence
//!
//! A live, unsuperseded memory that is not dormant (decisions and
//! preferences never are), not sensitive, not already a source, and created
//! after the artifact's last write. Record time (`created_at`), not event
//! time: the question is what the artifact's writer could have seen.
//!
//! # Cost
//!
//! One lookup of the sources' entities, then one link-index read per kept
//! entity, at most [`CANDIDATE_CAP`] candidate rows per artifact in all.

use crate::db::entities::Entities;
use crate::db::Result;
use crate::db::Store;
use crate::entity::normalize_entity_name;
use crate::models::Memory;
use crate::vitality::{effective_vitality, is_dormant, never_dormant};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Most `new_evidence` ids one check returns, newest first.
pub const NEW_EVIDENCE_MAX: usize = 5;

/// An entity linked to more memories than this is a hub and is skipped
/// (D88): it relates everything, so it says nothing about one artifact.
pub const HUB_LINK_CAP: usize = 250;

/// Most candidate rows one check reads across all its entities.
pub const CANDIDATE_CAP: usize = 500;

/// Entity kind that names a project (ADR-0027).
const PROJECT_KIND: &str = "project";

/// Whether newer evidence has arrived since an artifact was written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staleness {
    /// `None`: the artifact names no sources, so it cannot be checked.
    pub stale: Option<bool>,
    /// Up to [`NEW_EVIDENCE_MAX`] newer related memories, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_evidence: Vec<String>,
}

impl Staleness {
    /// The verdict for an artifact with no sources.
    pub fn unchecked() -> Self {
        Self::default()
    }

    /// Whether the check found newer evidence.
    pub fn is_stale(&self) -> bool {
        self.stale == Some(true)
    }
}

/// Check an artifact built from `sources` and last written at `last_write`
/// (RFC 3339) for newer evidence.
///
/// An unparseable `last_write` is read as the epoch, so every related memory
/// counts as newer: an artifact whose age is unknown is not vouched for.
pub fn stale_check(
    store: &Store<'_>,
    sources: &[String],
    last_write: &str,
    now: DateTime<Utc>,
) -> Result<Staleness> {
    if sources.is_empty() {
        return Ok(Staleness::unchecked());
    }
    let written = parse_time(last_write).unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let source_rows = crate::promotion::load_memories(store, sources)?;
    let projects: HashSet<String> = source_rows
        .iter()
        .filter_map(|m| m.project.clone())
        .collect();
    let source_ids: HashSet<&str> = sources.iter().map(String::as_str).collect();

    let entities = Entities::new(store);
    let mut evidence: Vec<(DateTime<Utc>, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut read = 0usize;
    for entity_id in entities.entities_of(sources)? {
        if read >= CANDIDATE_CAP {
            break;
        }
        if skipped_entity(&entities, &entity_id, &projects)? {
            continue;
        }
        for memory in entities.linked_memories_full(&entity_id)? {
            if read >= CANDIDATE_CAP {
                break;
            }
            read += 1;
            if source_ids.contains(memory.id.as_str()) || seen.contains(&memory.id) {
                continue;
            }
            let Some(created) = newer_evidence(&memory, written, &projects, now) else {
                continue;
            };
            seen.insert(memory.id.clone());
            evidence.push((created, memory.id));
        }
    }

    evidence.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    evidence.truncate(NEW_EVIDENCE_MAX);
    Ok(Staleness {
        stale: Some(!evidence.is_empty()),
        new_evidence: evidence.into_iter().map(|(_, id)| id).collect(),
    })
}

/// Whether `entity_id` is a project or hub entity, which the check skips.
fn skipped_entity(
    entities: &Entities<'_>,
    entity_id: &str,
    projects: &HashSet<String>,
) -> Result<bool> {
    let Some(entity) = entities.get(entity_id)? else {
        return Ok(true);
    };
    if entity.kind.as_deref() == Some(PROJECT_KIND) {
        return Ok(true);
    }
    let name = normalize_entity_name(&entity.name);
    if projects.iter().any(|p| normalize_entity_name(p) == name) {
        return Ok(true);
    }
    Ok(entities.linked_memory_count(entity_id)? > HUB_LINK_CAP)
}

/// `memory`'s creation time when it counts as newer evidence.
///
/// The caller has already dropped sources; the link read has already dropped
/// deleted and superseded rows.
fn newer_evidence(
    memory: &Memory,
    written: DateTime<Utc>,
    projects: &HashSet<String>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if memory.sensitive || memory.superseded_by.is_some() {
        return None;
    }
    if is_dormant(effective_vitality(memory, now)) && !never_dormant(memory) {
        return None;
    }
    if let (false, Some(project)) = (projects.is_empty(), memory.project.as_ref()) {
        if !projects.contains(project) {
            return None;
        }
    }
    let created = parse_time(&memory.created_at)?;
    (created > written).then_some(created)
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// [`stale_check`] for a wiki page, now: its sources are the ids it cites,
/// its last write its `updated_at`. A page that cites nothing is unchecked.
pub fn wiki_page_staleness(store: &Store<'_>, page: &crate::wiki::WikiPage) -> Result<Staleness> {
    stale_check(
        store,
        &cited_ids(&page.content),
        &page.updated_at,
        Utc::now(),
    )
}

/// The memory ids a wiki page cites (`mem_` followed by hex), in first-seen
/// order without repeats: its sources.
pub fn cited_ids(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (start, _) in content.match_indices("mem_") {
        let preceded_by_word = content[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if preceded_by_word {
            continue;
        }
        let hex: String = content[start + 4..]
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        if hex.len() < 8 {
            continue;
        }
        let id = format!("mem_{hex}");
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}
