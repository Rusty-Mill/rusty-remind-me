//! Browsing and curating the topic tree (`docs/design/topic-taxonomy.md`
//! §3 and §4).
//!
//! - [`level`] is one zoom step of the graph: a topic's children (or the top
//!   level) with their member counts, read by `GET /api/topics`.
//! - [`report`] is the rebalance check. It only reports: leaves that grew
//!   past the split size, small leaves with the sibling they would merge
//!   into, and how many knowledge memories have no topic. A person acts on
//!   it with the operations below.
//! - [`merge`], [`rename`], [`retire`] and `topic::set` (a move) change the
//!   tree. Each is a sequence of claim writes and link changes that is safe to
//!   run again if it stops part way: a re-run finishes what is left.

use crate::db::memories::Memories;
use crate::db::Store;
use crate::topic::{self, Parent, SetInput, Status, Topic, TopicName, TopicTree};
use crate::topic_assign::{active_leaves, Centroids};
use crate::topic_seed::{dot, is_seedable, SeedParams};
use serde::Serialize;
use std::collections::HashSet;

/// One topic in a [`Level`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LevelTopic {
    pub topic: String,
    pub label: String,
    pub status: Status,
    /// Memories assigned directly.
    pub members: usize,
    /// Distinct memories in this topic and everything below it.
    pub total: usize,
    /// Topics directly below it.
    pub children: usize,
}

/// One zoom step: the topics directly under `parent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Level {
    pub parent: String,
    pub topics: Vec<LevelTopic>,
}

/// The topics directly under `parent` (`None` or `root` for the top),
/// retired ones left out unless `include_retired`, largest first.
pub fn level(
    store: &Store<'_>,
    parent: Option<&str>,
    include_retired: bool,
) -> Result<Level, String> {
    let parent = match parent.map(str::trim).filter(|p| !p.is_empty()) {
        None => Parent::Root,
        Some(p) => Parent::parse(p).map_err(|e| e.to_string())?,
    };
    let tree = topic::tree(store).map_err(|e| e.to_string())?;
    if let Parent::Topic(name) = &parent {
        if tree.get(name).is_none() {
            return Err(format!("'{name}' is not a valid topic"));
        }
    }
    let memories = Memories::new(store);
    let mut topics = Vec::new();
    for t in tree.children(&parent) {
        if t.status == Status::Retired && !include_retired {
            continue;
        }
        let mut all = HashSet::new();
        for below in tree.subtree(&t.name) {
            all.extend(
                memories
                    .linked_ids(&below.entity_id)
                    .map_err(|e| e.to_string())?,
            );
        }
        topics.push(LevelTopic {
            topic: t.name.as_str().to_string(),
            label: t.name.label().to_string(),
            status: t.status,
            members: t.member_count,
            total: all.len(),
            children: tree.children(&Parent::Topic(t.name.clone())).count(),
        });
    }
    topics.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.label.cmp(&b.label)));
    Ok(Level {
        parent: match parent {
            Parent::Root => topic::ROOT.to_string(),
            Parent::Topic(n) => n.as_str().to_string(),
        },
        topics,
    })
}

/// A leaf that grew past the split size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Oversized {
    pub topic: String,
    pub members: usize,
}

/// A small leaf, and the sibling leaf whose centroid is closest to its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Undersized {
    pub topic: String,
    pub members: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_into: Option<String>,
}

/// What [`report`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub oversized: Vec<Oversized>,
    pub undersized: Vec<Undersized>,
    /// Knowledge memories with no topic.
    pub unplaced: usize,
}

/// Leaves under this many members are reported as merge candidates.
pub const SMALL_LEAF: usize = 10;

/// The rebalance check. Leaves over the seed's `leaf_max` are oversized;
/// leaves under [`SMALL_LEAF`] are undersized, with the closest sibling
/// leaf by centroid when centroids exist.
pub fn report(store: &Store<'_>) -> Result<Report, String> {
    let tree = topic::tree(store).map_err(|e| e.to_string())?;
    let centroids = crate::topic_assign::load(store)?.unwrap_or_default();
    let leaf_max = SeedParams::default().leaf_max;
    let mut out = Report::default();
    for leaf in active_leaves(&tree) {
        if leaf.member_count > leaf_max {
            out.oversized.push(Oversized {
                topic: leaf.name.as_str().to_string(),
                members: leaf.member_count,
            });
        } else if leaf.member_count < SMALL_LEAF {
            out.undersized.push(Undersized {
                topic: leaf.name.as_str().to_string(),
                members: leaf.member_count,
                merge_into: closest_sibling(&tree, leaf, &centroids),
            });
        }
    }
    let memories = Memories::new(store);
    let mut placed = HashSet::new();
    for t in &tree.topics {
        placed.extend(
            memories
                .linked_ids(&t.entity_id)
                .map_err(|e| e.to_string())?,
        );
    }
    out.unplaced = memories
        .all_live()
        .map_err(|e| e.to_string())?
        .iter()
        .filter(|m| is_seedable(m) && !placed.contains(&m.id))
        .count();
    Ok(out)
}

fn closest_sibling(tree: &TopicTree, leaf: &Topic, centroids: &Centroids) -> Option<String> {
    let vector_of = |name: &TopicName| {
        centroids
            .topics
            .iter()
            .find(|c| c.topic == name.as_str())
            .map(|c| &c.vector)
    };
    let own = vector_of(&leaf.name)?;
    tree.children(&leaf.parent)
        .filter(|s| s.status == Status::Active && s.name != leaf.name)
        .filter_map(|s| vector_of(&s.name).map(|v| (dot(own, v), s)))
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, s)| s.name.as_str().to_string())
}

/// What a curation call changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CurateOutcome {
    pub topic: String,
    /// Memories moved to the surviving topic.
    pub members_moved: usize,
    /// Child topics moved under the surviving topic.
    pub children_moved: usize,
    /// Claim memories written.
    pub claims_written: usize,
}

fn set(
    store: &Store<'_>,
    topic: &TopicName,
    parent: Option<Parent>,
    status: Option<Status>,
    because: &str,
) -> Result<usize, String> {
    topic::set(
        store,
        &SetInput {
            topic: topic.clone(),
            parent,
            status,
            evidence: Vec::new(),
            because: Some(because.to_string()),
            confirmed_by: "human".into(),
        },
    )
    .map(|o| o.written.len())
    .map_err(|e| format!("{topic}: {e}"))
}

fn parse(name: &str) -> Result<TopicName, String> {
    TopicName::parse(name).map_err(|e| e.to_string())
}

fn require_because(because: &str) -> Result<&str, String> {
    let trimmed = because.trim();
    if trimmed.is_empty() {
        return Err("give a reason (because)".into());
    }
    Ok(trimmed)
}

/// Fold topic `from` into active topic `into`: its members and child topics
/// move to `into`, then `from` is retired. `into` must not be `from` or
/// below it.
pub fn merge(
    store: &Store<'_>,
    from: &str,
    into: &str,
    because: &str,
) -> Result<CurateOutcome, String> {
    let because = require_because(because)?;
    let (from, into) = (parse(from)?, parse(into)?);
    let tree = topic::tree(store).map_err(|e| e.to_string())?;
    let source = tree
        .get(&from)
        .ok_or_else(|| format!("'{from}' is not a valid topic"))?;
    let target = tree
        .get(&into)
        .ok_or_else(|| format!("'{into}' is not a valid topic"))?;
    if target.status != Status::Active {
        return Err(format!("'{into}' is not active"));
    }
    if tree.subtree(&from).iter().any(|t| t.name == target.name) {
        return Err(format!("'{into}' is '{from}' or below it"));
    }
    let mut outcome = CurateOutcome {
        topic: target.name.as_str().to_string(),
        ..CurateOutcome::default()
    };
    let children: Vec<TopicName> = tree
        .children(&Parent::Topic(source.name.clone()))
        .map(|c| c.name.clone())
        .collect();
    for child in children {
        outcome.claims_written += set(
            store,
            &child,
            Some(Parent::Topic(target.name.clone())),
            None,
            because,
        )?;
        outcome.children_moved += 1;
    }
    let members = Memories::new(store)
        .linked_ids(&source.entity_id)
        .map_err(|e| e.to_string())?;
    for id in members {
        topic::assign(store, &id, &target.name).map_err(|e| format!("{id}: {e}"))?;
        topic::unassign(store, &id, &source.name).map_err(|e| format!("{id}: {e}"))?;
        outcome.members_moved += 1;
    }
    outcome.claims_written += set(store, &source.name, None, Some(Status::Retired), because)?;
    Ok(outcome)
}

/// Give topic `from` the label `to`: a new active topic with the same
/// parent takes its members and children, and `from` is retired.
pub fn rename(
    store: &Store<'_>,
    from: &str,
    to: &str,
    because: &str,
) -> Result<CurateOutcome, String> {
    let because = require_because(because)?;
    let (from, to) = (parse(from)?, parse(to)?);
    let tree = topic::tree(store).map_err(|e| e.to_string())?;
    let source = tree
        .get(&from)
        .ok_or_else(|| format!("'{from}' is not a valid topic"))?;
    if tree.get(&to).is_some_and(|t| t.name != source.name) {
        return Err(format!("'{to}' already exists; merge into it instead"));
    }
    let created = set(
        store,
        &to,
        Some(source.parent.clone()),
        Some(Status::Active),
        because,
    )?;
    let mut outcome = merge(store, from.as_str(), to.as_str(), because)?;
    outcome.claims_written += created;
    Ok(outcome)
}

/// Retire `topic`. Refused while it has an active child: move or merge
/// those first. Its members keep their link, so history reads true.
pub fn retire(store: &Store<'_>, topic_name: &str, because: &str) -> Result<CurateOutcome, String> {
    let because = require_because(because)?;
    let name = parse(topic_name)?;
    let tree = topic::tree(store).map_err(|e| e.to_string())?;
    let found = tree
        .get(&name)
        .ok_or_else(|| format!("'{name}' is not a valid topic"))?;
    if tree
        .children(&Parent::Topic(found.name.clone()))
        .any(|c| c.status == Status::Active)
    {
        return Err(format!(
            "'{name}' has active subtopics; move or merge them first"
        ));
    }
    Ok(CurateOutcome {
        topic: found.name.as_str().to_string(),
        claims_written: set(store, &found.name, None, Some(Status::Retired), because)?,
        ..CurateOutcome::default()
    })
}

/// Put `topic` under `parent` (`root` for the top level).
pub fn move_to(
    store: &Store<'_>,
    topic_name: &str,
    parent: &str,
    because: &str,
) -> Result<CurateOutcome, String> {
    let because = require_because(because)?;
    let name = parse(topic_name)?;
    let parent = Parent::parse(parent).map_err(|e| e.to_string())?;
    Ok(CurateOutcome {
        topic: name.as_str().to_string(),
        claims_written: set(store, &name, Some(parent), None, because)?,
        ..CurateOutcome::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::NewMemory;
    use crate::db::Database;

    fn make(store: &Store<'_>, label: &str, parent: &str) {
        set(
            store,
            &parse(label).unwrap(),
            Some(Parent::parse(parent).unwrap()),
            Some(Status::Active),
            "test",
        )
        .unwrap();
    }

    fn member(store: &Store<'_>, label: &str) -> String {
        let id = format!("mem_{}", uuid::Uuid::new_v4().simple());
        let mut row = NewMemory::new(id.clone(), "text", &chrono::Utc::now().to_rfc3339());
        row.category = "fact".into();
        Memories::new(store).insert(&row).unwrap();
        topic::assign(store, &id, &parse(label).unwrap()).unwrap();
        id
    }

    fn members_of(store: &Store<'_>, label: &str) -> HashSet<String> {
        Memories::new(store)
            .linked_ids(&parse(label).unwrap().entity_id())
            .unwrap()
    }

    #[test]
    fn a_level_counts_members_below_and_hides_retired() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        make(&store, "Engineering", "root");
        make(&store, "Systems", "Engineering");
        make(&store, "Cooking", "root");
        member(&store, "Engineering");
        member(&store, "Systems");
        member(&store, "Systems");
        member(&store, "Cooking");

        let top = level(&store, None, false).unwrap();
        let rows: Vec<(&str, usize, usize, usize)> = top
            .topics
            .iter()
            .map(|t| (t.label.as_str(), t.members, t.total, t.children))
            .collect();
        assert_eq!(rows, [("Engineering", 1, 3, 1), ("Cooking", 1, 1, 0)]);
        assert_eq!(
            level(&store, Some("Engineering"), false)
                .unwrap()
                .topics
                .len(),
            1
        );
        assert!(level(&store, Some("Nowhere"), false).is_err());

        retire(&store, "Cooking", "unused").unwrap();
        assert_eq!(level(&store, Some("root"), false).unwrap().topics.len(), 1);
        assert_eq!(level(&store, Some("root"), true).unwrap().topics.len(), 2);
    }

    #[test]
    fn merge_moves_members_and_children_then_retires() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        make(&store, "Software", "root");
        make(&store, "Coding", "root");
        make(&store, "Testing", "Coding");
        let a = member(&store, "Coding");
        let b = member(&store, "Software");

        let out = merge(&store, "Coding", "Software", "same thing").unwrap();
        assert_eq!((out.members_moved, out.children_moved), (1, 1));
        assert_eq!(members_of(&store, "Software"), HashSet::from([a, b]));
        assert!(members_of(&store, "Coding").is_empty());
        let tree = topic::tree(&store).unwrap();
        assert_eq!(
            tree.get(&parse("Coding").unwrap()).unwrap().status,
            Status::Retired
        );
        assert_eq!(
            tree.get(&parse("Testing").unwrap()).unwrap().parent,
            Parent::Topic(parse("Software").unwrap())
        );

        assert!(
            merge(&store, "Software", "Testing", "x").is_err(),
            "into its own child"
        );
        assert!(
            merge(&store, "Software", "Coding", "x").is_err(),
            "into a retired topic"
        );
        assert!(merge(&store, "Software", "Nowhere", "x").is_err());
        assert!(
            merge(&store, "Testing", "Software", " ").is_err(),
            "no reason"
        );
    }

    #[test]
    fn rename_carries_members_children_and_parent() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        make(&store, "Engineering", "root");
        make(&store, "Sys Eng", "Engineering");
        make(&store, "MBSE", "Sys Eng");
        let m = member(&store, "Sys Eng");

        rename(&store, "Sys Eng", "Systems Engineering", "full name").unwrap();
        let tree = topic::tree(&store).unwrap();
        let new = tree.get(&parse("Systems Engineering").unwrap()).unwrap();
        assert_eq!(new.parent, Parent::Topic(parse("Engineering").unwrap()));
        assert_eq!(new.status, Status::Active);
        assert!(members_of(&store, "Systems Engineering").contains(&m));
        assert_eq!(
            tree.get(&parse("MBSE").unwrap()).unwrap().parent,
            Parent::Topic(parse("Systems Engineering").unwrap())
        );
        assert!(
            rename(&store, "MBSE", "Engineering", "x").is_err(),
            "name taken"
        );
    }

    #[test]
    fn retire_and_move_guard_the_tree() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        make(&store, "A", "root");
        make(&store, "B", "A");
        assert!(retire(&store, "A", "x").is_err(), "has an active child");
        assert_eq!(
            move_to(&store, "B", "root", "flatten")
                .unwrap()
                .claims_written,
            1
        );
        assert!(move_to(&store, "A", "A", "x").is_err());
        assert_eq!(retire(&store, "A", "empty").unwrap().claims_written, 1);
    }

    #[test]
    fn the_report_flags_small_leaves_and_counts_the_unplaced() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        make(&store, "Tiny", "root");
        member(&store, "Tiny");
        let mut row = NewMemory::new("mem_loose", "loose", &chrono::Utc::now().to_rfc3339());
        row.category = "fact".into();
        Memories::new(&store).insert(&row).unwrap();

        let r = report(&store).unwrap();
        assert!(r.oversized.is_empty());
        assert_eq!(r.undersized.len(), 1);
        assert_eq!(r.undersized[0].merge_into, None, "no centroids yet");
        assert_eq!(r.unplaced, 1);
    }
}
