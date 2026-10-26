//! What the store suggests about a project, and how a person answers it
//! (ADR-0027 section 5).
//!
//! Proposals are computed when read, like [`crate::promotion`]'s candidates:
//! nothing is stored, so a rule that improves does not leave stale rows
//! behind. Every rule is deterministic and calls no model, and every one
//! carries the memories it read as the evidence a confirm will cite.
//!
//! A person answers. [`confirm`] writes the claim through
//! [`crate::project::set`]; [`reject`] records the proposal's key so it is not
//! shown again. A rejection is an ordinary memory, so it syncs, and a
//! different value gets a different key — new evidence for a different stage
//! is still offered.

use crate::db::entities::Entities;
use crate::db::memories::{Memories, NewMemory};
use crate::db::{Result, Store, StoreError};
use crate::entity::{normalize_entity_name, Entity};
use crate::models::Memory;
use crate::project::{
    self, ClaimValue, Predicate, RepoPath, RepoUrl, SetInput, Stage, Status, WriteOutcome,
};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::HashSet;

/// Tag on a rejection memory.
pub const REJECTED_TAG: &str = "project-proposal-dismissed";
/// `source` of a rejection memory.
const SOURCE: &str = "project";
/// Mentions that make an unknown entity worth proposing as a project.
pub const MIN_MENTIONS: usize = 3;
/// Entity kinds that make an entity a project outright.
pub const PROJECT_KINDS: [&str; 4] = ["project", "app", "crate", "repo"];
/// Tags on a design decision that moves an idea to `designed`.
const DESIGN_TAGS: [&str; 2] = ["design", "adr"];
/// How long without a linked memory makes an active project dormant.
pub const DORMANT_AFTER_DAYS: i64 = 60;
/// Env var overriding [`DORMANT_AFTER_DAYS`].
pub const DORMANT_AFTER_ENV: &str = "REMIND_ME_PROJECT_DORMANT_DAYS";

/// Which rule produced a proposal, so a reader can judge it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// Mentioned, or kinded as a project, with nothing built.
    MentionedNotBuilt,
    /// A session's git remote names it.
    RemoteSeen,
    /// A session's working directory sits inside it.
    PathSeen,
    /// A design decision exists for an idea.
    DesignDecision,
    /// Nothing has mentioned it for a while.
    GoneQuiet,
}

/// One suggestion about one project, with the evidence behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Proposal {
    /// Stable across reads: `project:<predicate>:<value>`, normalised.
    pub key: String,
    pub project: String,
    pub predicate: Predicate,
    pub value: String,
    pub rule: Rule,
    pub evidence: Vec<String>,
    /// Set together with `stage` when the project is new.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_status: Option<Status>,
    pub why: String,
}

impl Proposal {
    fn new(
        project: &str,
        value: ClaimValue,
        rule: Rule,
        evidence: Vec<String>,
        why: String,
    ) -> Self {
        let object = value.object();
        Self {
            key: proposal_key(project, value.predicate(), &object),
            project: project.to_string(),
            predicate: value.predicate(),
            value: object,
            rule,
            evidence,
            with_status: None,
            why,
        }
    }

    /// The claims a confirm of this proposal writes.
    fn claims(&self) -> Result<Vec<ClaimValue>> {
        let mut claims = vec![ClaimValue::parse(self.predicate, &self.value)?];
        if let Some(status) = self.with_status {
            claims.push(ClaimValue::Status(status));
        }
        Ok(claims)
    }
}

/// The key a rejection stores, so the same suggestion is not offered twice.
pub fn proposal_key(project: &str, predicate: Predicate, value: &str) -> String {
    format!(
        "{}:{}:{}",
        normalize_entity_name(project),
        predicate.as_str(),
        normalize_entity_name(value)
    )
}

/// Every proposal the store supports, minus the rejected ones.
///
/// Ordered by project then predicate, so two reads of an unchanged store
/// return the same list.
pub fn proposals(store: &Store<'_>) -> Result<Vec<Proposal>> {
    let rejected = rejected_keys(store)?;
    let entities = Entities::new(store);
    let now = Utc::now();
    let dormant_after = Duration::days(dormant_after_days());

    let mut out = Vec::new();
    for entity in entities.all()? {
        let linked = entities.linked_memories_full(&entity.id)?;
        let live = project::get(store, &entity.name).ok().flatten();
        let stage = live.as_ref().map(|p| p.stage.value);
        let status = live.as_ref().map(|p| p.status.value);
        let anchored_repo = live.as_ref().and_then(|p| p.anchors.repo.clone());
        let anchored_path = live.as_ref().is_some_and(|p| p.anchors.path.is_some());

        if let Some(found) = remote_evidence(&entity, &linked) {
            // The remote is the strongest evidence there is a repository, so
            // it proposes `built` as well as the anchor.
            if anchored_repo.as_ref().map(|c| c.value.clone()) != Some(found.url.clone()) {
                out.push(Proposal::new(
                    &entity.name,
                    ClaimValue::Repo(found.url.clone()),
                    Rule::RemoteSeen,
                    found.evidence.clone(),
                    format!("{} sessions ran against this remote", found.evidence.len()),
                ));
            }
            if stage != Some(Stage::Built) {
                let mut proposal = Proposal::new(
                    &entity.name,
                    ClaimValue::Stage(Stage::Built),
                    Rule::RemoteSeen,
                    found.evidence,
                    "a session's git remote names this project".to_string(),
                );
                proposal.with_status = (status.is_none()).then_some(Status::Active);
                out.push(proposal);
            }
        } else if stage.is_none() && mentioned_enough(&entity, &linked) {
            let mut proposal = Proposal::new(
                &entity.name,
                ClaimValue::Stage(Stage::Idea),
                Rule::MentionedNotBuilt,
                evidence_ids(&linked),
                format!(
                    "mentioned by {} memories, with no repository seen",
                    linked.len()
                ),
            );
            proposal.with_status = Some(Status::Active);
            out.push(proposal);
        }

        if !anchored_path {
            if let Some((path, evidence)) = path_evidence(&entity, &linked) {
                out.push(Proposal::new(
                    &entity.name,
                    ClaimValue::Path(path),
                    Rule::PathSeen,
                    evidence,
                    "a session worked inside this directory".to_string(),
                ));
            }
        }

        if stage == Some(Stage::Idea) {
            if let Some(decision) = design_decision(&linked) {
                out.push(Proposal::new(
                    &entity.name,
                    ClaimValue::Stage(Stage::Designed),
                    Rule::DesignDecision,
                    vec![decision.id.clone()],
                    "a design decision was recorded for it".to_string(),
                ));
            }
        }

        if status == Some(Status::Active) {
            if let Some(last) = newest(&linked) {
                if is_older_than(&last.created_at, now, dormant_after) {
                    out.push(Proposal::new(
                        &entity.name,
                        ClaimValue::Status(Status::Dormant),
                        Rule::GoneQuiet,
                        vec![last.id.clone()],
                        format!("nothing has mentioned it since {}", last.created_at),
                    ));
                }
            }
        }
    }

    out.retain(|proposal| !rejected.contains(&proposal.key));
    out.sort_by(|a, b| {
        (
            normalize_entity_name(&a.project),
            a.predicate,
            a.value.clone(),
        )
            .cmp(&(
                normalize_entity_name(&b.project),
                b.predicate,
                b.value.clone(),
            ))
    });
    out.dedup_by(|a, b| a.key == b.key);
    Ok(out)
}

/// Accept the proposal `key`, writing its claims with its evidence.
pub fn confirm(store: &Store<'_>, key: &str, confirmed_by: &str) -> Result<WriteOutcome> {
    let proposal = find(store, key)?;
    project::set(
        store,
        &SetInput {
            project: proposal.project.clone(),
            claims: proposal.claims()?,
            evidence: proposal.evidence.clone(),
            because: None,
            confirmed_by: confirmed_by.to_string(),
            proposal: Some(proposal.key.clone()),
        },
    )
}

/// Decline the proposal `key` so it is not offered again. Returns the
/// rejection memory's id, or the existing one when it was already rejected.
pub fn reject(store: &Store<'_>, key: &str, rejected_by: &str) -> Result<String> {
    // The stored rejection first: once rejected, the proposal is no longer
    // open, so looking it up again would refuse a repeated reject.
    if let Some(id) = rejection_id(store, key)? {
        return Ok(id);
    }
    let proposal = find(store, key)?;
    let now = Utc::now().to_rfc3339();
    let row = NewMemory {
        category: crate::kinds::MemoryKind::WorkLog.as_str().to_string(),
        memory_type: crate::kinds::MemoryKind::WorkLog.as_str().to_string(),
        tags: vec![REJECTED_TAG.to_string()],
        source: SOURCE.to_string(),
        metadata: serde_json::json!({
            "project_proposal": {
                "key": proposal.key,
                "project": proposal.project,
                "predicate": proposal.predicate.as_str(),
                "value": proposal.value,
                "rejected_by": rejected_by,
            }
        }),
        ..crate::context::default_provenance().stamp(NewMemory::new(
            format!("mem_{}", uuid::Uuid::new_v4().simple()),
            format!(
                "Rejected proposal: {} {} = {}",
                proposal.project,
                proposal.predicate.qualified(),
                proposal.value
            ),
            &now,
        ))
    };
    Memories::new(store).insert(&row)?;
    Ok(row.id)
}

fn find(store: &Store<'_>, key: &str) -> Result<Proposal> {
    proposals(store)?
        .into_iter()
        .find(|p| p.key == key)
        .ok_or_else(|| StoreError::Invalid(format!("no open proposal '{key}'")))
}

/// Keys of every rejection, so the listing can drop them.
fn rejected_keys(store: &Store<'_>) -> Result<HashSet<String>> {
    Ok(rejections(store)?.filter_map(|(_, key)| key).collect())
}

fn rejection_id(store: &Store<'_>, key: &str) -> Result<Option<String>> {
    Ok(rejections(store)?
        .find(|(_, stored)| stored.as_deref() == Some(key))
        .map(|(id, _)| id))
}

fn rejections(store: &Store<'_>) -> Result<impl Iterator<Item = (String, Option<String>)>> {
    Ok(Memories::new(store)
        .all_live()?
        .into_iter()
        .filter(|m| m.tags.iter().any(|t| t == REJECTED_TAG))
        .map(|m| {
            let key = m
                .metadata
                .get("project_proposal")
                .and_then(|p| p.get("key"))
                .and_then(|k| k.as_str())
                .map(str::to_string);
            (m.id, key)
        }))
}

/// How long silence makes a project dormant. A malformed override is ignored
/// rather than failing every read.
fn dormant_after_days() -> i64 {
    std::env::var(DORMANT_AFTER_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|days| *days > 0)
        .unwrap_or(DORMANT_AFTER_DAYS)
}

fn evidence_ids(linked: &[Memory]) -> Vec<String> {
    linked.iter().map(|m| m.id.clone()).collect()
}

fn newest(linked: &[Memory]) -> Option<&Memory> {
    // `linked_memories_full` is newest first.
    linked.first()
}

fn is_older_than(created_at: &str, now: DateTime<Utc>, by: Duration) -> bool {
    DateTime::parse_from_rfc3339(created_at)
        .is_ok_and(|at| now.signed_duration_since(at.with_timezone(&Utc)) > by)
}

fn mentioned_enough(entity: &Entity, linked: &[Memory]) -> bool {
    let kinded = entity
        .kind
        .as_deref()
        .is_some_and(|k| PROJECT_KINDS.contains(&k.trim().to_ascii_lowercase().as_str()));
    !linked.is_empty() && (kinded || linked.len() >= MIN_MENTIONS)
}

/// Every name an entity answers to, normalised.
fn names(entity: &Entity) -> Vec<String> {
    std::iter::once(&entity.name)
        .chain(entity.aliases.iter())
        .map(|n| normalize_entity_name(n))
        .filter(|n| !n.is_empty())
        .collect()
}

struct RemoteEvidence {
    url: RepoUrl,
    evidence: Vec<String>,
}

/// A repository whose last path segment matches the entity's name or an alias.
///
/// Matching on the repository's own name, not on the memory's `project`
/// field: the field holds whatever directory a session ran in, which is how a
/// crate inside a monorepo would otherwise claim the monorepo's remote.
fn remote_evidence(entity: &Entity, linked: &[Memory]) -> Option<RemoteEvidence> {
    let names = names(entity);
    let mut found: Option<RepoUrl> = None;
    let mut evidence = Vec::new();
    for memory in linked {
        let Some(url) = memory
            .git_remote
            .as_deref()
            .and_then(|r| RepoUrl::parse(r).ok())
        else {
            continue;
        };
        let repo_name = url.as_str().rsplit('/').next().unwrap_or_default();
        if !names.contains(&normalize_entity_name(repo_name)) {
            continue;
        }
        // One remote only: a second, different one is a contradiction a
        // person resolves with `set`, not something a rule guesses at.
        match &found {
            Some(seen) if *seen != url => return None,
            Some(_) => {}
            None => found = Some(url),
        }
        evidence.push(memory.id.clone());
    }
    found.map(|url| RemoteEvidence { url, evidence })
}

/// A path under the repository root whose last segment names the entity.
fn path_evidence(entity: &Entity, linked: &[Memory]) -> Option<(RepoPath, Vec<String>)> {
    let names = names(entity);
    let mut found: Option<RepoPath> = None;
    let mut evidence = Vec::new();
    for memory in linked {
        let Some(cwd) = memory.cwd.as_deref() else {
            continue;
        };
        let normalized = cwd.replace('\\', "/");
        let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
        let Some(last) = segments.last() else {
            continue;
        };
        if !names.contains(&normalize_entity_name(last)) {
            continue;
        }
        // Keep the repository-relative tail: `crates/<group>/<name>`, the
        // layout this workspace uses. Anything else is too shallow to anchor.
        let crates_at = segments.iter().position(|s| *s == "crates")?;
        let tail = segments[crates_at..].join("/");
        let Ok(path) = RepoPath::parse(&tail) else {
            continue;
        };
        match &found {
            Some(seen) if *seen != path => return None,
            Some(_) => {}
            None => found = Some(path),
        }
        evidence.push(memory.id.clone());
    }
    found.map(|path| (path, evidence))
}

/// A decision memory tagged as design work.
fn design_decision(linked: &[Memory]) -> Option<&Memory> {
    linked.iter().find(|m| {
        m.memory_type.as_deref() == Some(crate::kinds::MemoryKind::Decision.as_str())
            && m.tags
                .iter()
                .any(|t| DESIGN_TAGS.contains(&t.trim().to_ascii_lowercase().as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::entity::{link_memory_entity, upsert_entity};
    use crate::models::EntityInput;

    /// A memory linked to `entity`, with the context fields a rule reads.
    fn linked(
        store: &Store<'_>,
        entity_id: &str,
        id: &str,
        edit: impl FnOnce(&mut NewMemory),
    ) -> String {
        let now = Utc::now().to_rfc3339();
        let mut row = NewMemory::new(id, "something happened", &now);
        row.category = crate::models::FACT_CATEGORY.to_string();
        edit(&mut row);
        Memories::new(store).insert(&row).unwrap();
        link_memory_entity(store, &row.id, entity_id).unwrap();
        row.id
    }

    fn entity(store: &Store<'_>, name: &str, kind: Option<&str>) -> Entity {
        upsert_entity(
            store,
            &EntityInput {
                name: name.to_string(),
                kind: kind.map(str::to_string),
                aliases: Vec::new(),
            },
        )
        .unwrap()
    }

    fn keys(store: &Store<'_>) -> Vec<String> {
        proposals(store)
            .unwrap()
            .into_iter()
            .map(|p| p.key)
            .collect()
    }

    #[test]
    fn keys_are_stable_and_normalised() {
        assert_eq!(
            proposal_key("  Rusty  Mux ", Predicate::Stage, " Built "),
            proposal_key("rusty mux", Predicate::Stage, "built")
        );
        assert_eq!(
            proposal_key("mill", Predicate::Stage, "built"),
            "mill:stage:built"
        );
    }

    #[test]
    fn mentions_without_a_repo_propose_an_idea() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_mux", None);
        for i in 0..2 {
            linked(&store, &e.id, &format!("mem_m{i}"), |_| {});
        }
        assert!(keys(&store).is_empty(), "two mentions is not enough");

        linked(&store, &e.id, "mem_m2", |_| {});
        let proposals = proposals(&store).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].predicate, Predicate::Stage);
        assert_eq!(proposals[0].value, "idea");
        assert_eq!(proposals[0].with_status, Some(Status::Active));
        assert_eq!(proposals[0].evidence.len(), 3);
    }

    #[test]
    fn a_project_kind_proposes_on_one_mention() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_tick", Some("App"));
        linked(&store, &e.id, "mem_one", |_| {});
        assert_eq!(keys(&store), ["rusty_tick:stage:idea"]);
    }

    #[test]
    fn a_matching_remote_proposes_built_and_the_repo() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_mill", None);
        linked(&store, &e.id, "mem_r", |row| {
            row.git_remote = Some("git@github.com:Rusty-Mill/rusty_mill.git".into());
        });
        let mut found = keys(&store);
        found.sort();
        assert_eq!(
            found,
            [
                "rusty_mill:repo:https://github.com/rusty-mill/rusty_mill",
                "rusty_mill:stage:built",
            ]
        );
    }

    #[test]
    fn a_remote_naming_another_repo_proposes_nothing() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        // A crate inside the monorepo: the session's remote is the
        // monorepo's, which says nothing about this crate having a repo.
        let e = entity(&store, "rusty_mux", None);
        linked(&store, &e.id, "mem_r", |row| {
            row.git_remote = Some("https://github.com/Rusty-Mill/rusty_mill".into());
        });
        assert!(keys(&store).is_empty());
    }

    #[test]
    fn two_different_remotes_propose_nothing() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_mux", None);
        linked(&store, &e.id, "mem_a", |row| {
            row.git_remote = Some("https://github.com/one/rusty_mux".into());
        });
        linked(&store, &e.id, "mem_b", |row| {
            row.git_remote = Some("https://github.com/two/rusty_mux".into());
        });
        assert!(keys(&store).is_empty(), "a contradiction is for a person");
    }

    #[test]
    fn a_working_directory_proposes_the_crate_path() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_mux", None);
        linked(&store, &e.id, "mem_c", |row| {
            row.cwd = Some("/home/nano/src/rusty_mill/crates/apps/rusty_mux".into());
        });
        assert!(keys(&store).contains(&"rusty_mux:path:crates/apps/rusty_mux".to_string()));
    }

    #[test]
    fn a_design_decision_moves_an_idea_to_designed() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_mux", None);
        let fact = linked(&store, &e.id, "mem_seen", |_| {});
        project::set(
            &store,
            &SetInput {
                project: "rusty_mux".into(),
                claims: vec![
                    ClaimValue::Stage(Stage::Idea),
                    ClaimValue::Status(Status::Active),
                ],
                evidence: vec![fact],
                because: None,
                confirmed_by: "human".into(),
                proposal: None,
            },
        )
        .unwrap();
        assert!(
            keys(&store).is_empty(),
            "an idea with one fact proposes nothing"
        );

        linked(&store, &e.id, "mem_adr", |row| {
            row.memory_type = "decision".into();
            row.tags = vec!["ADR".into()];
            row.metadata = serde_json::json!({ "rationale": "it is worth building" });
        });
        assert_eq!(keys(&store), ["rusty_mux:stage:designed"]);
    }

    #[test]
    fn silence_proposes_dormant() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_mux", None);
        let old = (Utc::now() - Duration::days(90)).to_rfc3339();
        let fact = linked(&store, &e.id, "mem_old", |row| {
            row.created_at = old.clone();
            row.updated_at = old.clone();
        });
        project::set(
            &store,
            &SetInput {
                project: "rusty_mux".into(),
                claims: vec![
                    ClaimValue::Stage(Stage::Idea),
                    ClaimValue::Status(Status::Active),
                ],
                evidence: vec![fact.clone()],
                because: None,
                confirmed_by: "human".into(),
                proposal: None,
            },
        )
        .unwrap();
        let dormant = "rusty_mux:status:dormant".to_string();
        assert!(keys(&store).contains(&dormant));

        // The claim memories this project just wrote are themselves recent,
        // but they are not linked mentions, so they do not reset the clock.
        crate::test_env::set_var(DORMANT_AFTER_ENV, "365");
        assert!(!keys(&store).contains(&dormant));
        crate::test_env::remove_var(DORMANT_AFTER_ENV);
    }

    #[test]
    fn confirm_writes_the_claim_with_the_proposal_evidence() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_mill", None);
        let evidence = linked(&store, &e.id, "mem_r", |row| {
            row.git_remote = Some("https://github.com/Rusty-Mill/rusty_mill".into());
        });

        let built = keys(&store)
            .into_iter()
            .find(|k| k.contains("stage"))
            .unwrap();
        confirm(&store, &built, "human").unwrap();

        let project = project::get(&store, "rusty_mill").unwrap().unwrap();
        assert_eq!(project.stage.value, Stage::Built);
        assert_eq!(project.status.value, Status::Active);
        assert_eq!(project.stage.evidence.ids(), [evidence.as_str()]);
        assert!(
            !keys(&store).iter().any(|k| *k == built),
            "confirmed, so no longer open"
        );
        assert!(
            confirm(&store, &built, "human").is_err(),
            "not open any more"
        );
    }

    #[test]
    fn reject_hides_only_that_value() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let e = entity(&store, "rusty_mux", None);
        for i in 0..3 {
            linked(&store, &e.id, &format!("mem_m{i}"), |_| {});
        }
        let idea = "rusty_mux:stage:idea".to_string();
        let id = reject(&store, &idea, "human").unwrap();
        assert_eq!(reject(&store, &idea, "human").unwrap(), id, "idempotent");
        assert!(!keys(&store).contains(&idea));

        // Different evidence for a different value is still offered.
        linked(&store, &e.id, "mem_r", |row| {
            row.git_remote = Some("https://github.com/Rusty-Mill/rusty_mux".into());
        });
        assert!(keys(&store).contains(&"rusty_mux:stage:built".to_string()));
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        assert!(confirm(&store, "nope:stage:built", "human").is_err());
        assert!(reject(&store, "nope:stage:built", "human").is_err());
    }
}
