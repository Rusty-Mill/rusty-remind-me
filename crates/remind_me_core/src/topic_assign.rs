//! Assigning new memories to topics (`docs/design/topic-taxonomy.md` §3).
//!
//! Each active leaf topic has a centroid: the unit mean of its members'
//! vectors. A new refined memory joins the closest leaf when the cosine is
//! at least the match floor, and every other leaf within [`MARGIN`] of the
//! best, at most [`MAX_TOPICS`]. Below the floor it gets no topic and waits
//! for a later sweep or a rebalance.
//!
//! Centroids are a derived cache, `<store>.topics/centroids.json`, built
//! explicitly ([`rebuild`], run by `topics apply` and `topics assign`) and
//! never on a write: the write path only reads it. A missing file or one of
//! another dimension means no assignment, never an error, the same rule the
//! ANN index follows.

use crate::db::memories::Memories;
use crate::db::vectors::Vectors;
use crate::db::Store;
use crate::topic::{self, Parent, Status, TopicName};
use crate::topic_seed::{dot, is_seedable, normalize, snapshot_dir};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Cosine, above 0 and at most 1, a memory needs to join a topic.
pub const MATCH_ENV: &str = "REMIND_ME_TOPIC_MATCH";
/// [`MATCH_ENV`] when unset.
pub const DEFAULT_MATCH: f32 = 0.5;
/// How far below the best match another topic may be and still be joined.
pub const MARGIN: f32 = 0.05;
/// Most topics one memory joins at assignment.
pub const MAX_TOPICS: usize = 3;
const FILE: &str = "centroids.json";

/// One leaf topic's centroid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Centroid {
    pub topic: String,
    pub vector: Vec<f32>,
}

/// Every active leaf's centroid, all of one dimension.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Centroids {
    pub dim: usize,
    pub topics: Vec<Centroid>,
}

/// The topics `vector` joins: the best at or above `floor`, then any within
/// [`MARGIN`] of it, best first, at most [`MAX_TOPICS`]. Pure.
pub fn choose(centroids: &Centroids, vector: &[f32], floor: f32) -> Vec<String> {
    if vector.len() != centroids.dim {
        return Vec::new();
    }
    let mut scored: Vec<(f32, &str)> = centroids
        .topics
        .iter()
        .map(|c| (dot(vector, &c.vector), c.topic.as_str()))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let Some(&(best, _)) = scored.first() else {
        return Vec::new();
    };
    if best < floor {
        return Vec::new();
    }
    scored
        .into_iter()
        .take_while(|(s, _)| *s >= best - MARGIN)
        .take(MAX_TOPICS)
        .map(|(_, t)| t.to_string())
        .collect()
}

/// A memory's unit vector: the normalised mean of its chunks. `None` when it
/// has none, or they disagree on dimension.
pub fn memory_vector(store: &Store<'_>, memory_id: &str) -> Result<Option<Vec<f32>>, String> {
    let chunks = Vectors::new(store)
        .chunks_for(memory_id)
        .map_err(|e| e.to_string())?;
    let mut sum: Option<Vec<f32>> = None;
    for chunk in chunks {
        let v = crate::vectors::le_bytes_to_f32(&chunk.embedding);
        match &mut sum {
            None => sum = Some(v),
            Some(s) if s.len() == v.len() => s.iter_mut().zip(&v).for_each(|(a, b)| *a += b),
            Some(_) => return Ok(None),
        }
    }
    Ok(sum.filter(|s| !s.is_empty()).map(normalize))
}

/// Active topics with no active child.
pub(crate) fn active_leaves(tree: &topic::TopicTree) -> Vec<&topic::Topic> {
    tree.topics
        .iter()
        .filter(|t| t.status == Status::Active)
        .filter(|t| {
            !tree
                .children(&Parent::Topic(t.name.clone()))
                .any(|c| c.status == Status::Active)
        })
        .collect()
}

/// Compute every active leaf's centroid from its members' vectors and save
/// them. A leaf with no embedded member has no centroid.
pub fn rebuild(store: &Store<'_>) -> Result<Centroids, String> {
    let tree = topic::tree(store).map_err(|e| e.to_string())?;
    let memories = Memories::new(store);
    let mut out = Centroids::default();
    for leaf in active_leaves(&tree) {
        let members = memories
            .linked_ids(&leaf.entity_id)
            .map_err(|e| e.to_string())?;
        let mut sum: Vec<f32> = Vec::new();
        for id in &members {
            let Some(v) = memory_vector(store, id)? else {
                continue;
            };
            if sum.is_empty() {
                sum = vec![0.0; v.len()];
            }
            if v.len() == sum.len() {
                sum.iter_mut().zip(&v).for_each(|(a, b)| *a += b);
            }
        }
        if sum.is_empty() || (out.dim != 0 && sum.len() != out.dim) {
            continue;
        }
        out.dim = sum.len();
        out.topics.push(Centroid {
            topic: leaf.name.as_str().to_string(),
            vector: normalize(sum),
        });
    }
    save(&snapshot_dir(store)?, &out)?;
    Ok(out)
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

fn save(dir: &Path, centroids: &Centroids) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let target = path(dir);
    let tmp = target.with_extension("json.tmp");
    let text = serde_json::to_string(centroids).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &target).map_err(|e| format!("cannot write {}: {e}", target.display()))
}

/// The saved centroids, `None` when there are none yet.
pub fn load(store: &Store<'_>) -> Result<Option<Centroids>, String> {
    let Ok(dir) = snapshot_dir(store) else {
        return Ok(None);
    };
    let file = path(&dir);
    if !file.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{} is not a centroid file: {e}", file.display()))
}

/// [`MATCH_ENV`], or [`DEFAULT_MATCH`] when unset.
pub fn match_floor() -> Result<f32, String> {
    match std::env::var(MATCH_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        None => Ok(DEFAULT_MATCH),
        Some(v) => match v.trim().parse::<f32>() {
            Ok(n) if n > 0.0 && n <= 1.0 => Ok(n),
            _ => Err(format!(
                "{MATCH_ENV} must be above 0 and at most 1, got {v:?}"
            )),
        },
    }
}

/// Link memory `memory_id` to the topics its vector matches, using the saved
/// centroids. Returns the topics it joined; empty when there are no
/// centroids, the memory is not knowledge a topic holds, or nothing is close
/// enough.
pub fn assign_memory(
    store: &Store<'_>,
    memory_id: &str,
    centroids: &Centroids,
    floor: f32,
) -> Result<Vec<String>, String> {
    let Some(memory) = Memories::new(store)
        .get_live(memory_id)
        .map_err(|e| e.to_string())?
    else {
        return Ok(Vec::new());
    };
    if !is_seedable(&memory) {
        return Ok(Vec::new());
    }
    let Some(vector) = memory_vector(store, memory_id)? else {
        return Ok(Vec::new());
    };
    let mut joined = Vec::new();
    for name in choose(centroids, &vector, floor) {
        let topic = TopicName::parse(&name).map_err(|e| e.to_string())?;
        match topic::assign(store, memory_id, &topic) {
            Ok(_) => joined.push(name),
            // Retired or moved since the centroids were built: skip it.
            Err(crate::db::StoreError::Invalid(_)) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(joined)
}

/// The write path's hook: assign a just-written memory, logging (never
/// raising) a failure, since the write itself already succeeded.
pub fn assign_on_write(store: &Store<'_>, memory_id: &str) {
    let result = load(store).and_then(|centroids| match centroids {
        None => Ok(Vec::new()),
        Some(c) => assign_memory(store, memory_id, &c, match_floor()?),
    });
    if let Err(e) = result {
        eprintln!("rusty-remind-me: topic assignment for {memory_id} failed: {e}");
    }
}

/// What [`assign_pending`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignReport {
    /// Leaf topics with a centroid.
    pub centroids: usize,
    /// Memories examined: knowledge with a vector and no topic yet.
    pub examined: usize,
    /// Of those, how many joined at least one topic.
    pub assigned: usize,
}

/// Rebuild the centroids, then give every knowledge memory with a vector and
/// no topic its topics.
pub fn assign_pending(store: &Store<'_>) -> Result<AssignReport, String> {
    let centroids = rebuild(store)?;
    let floor = match_floor()?;
    let tree = topic::tree(store).map_err(|e| e.to_string())?;
    let memories = Memories::new(store);
    let mut placed = std::collections::HashSet::new();
    for t in &tree.topics {
        placed.extend(
            memories
                .linked_ids(&t.entity_id)
                .map_err(|e| e.to_string())?,
        );
    }
    let mut report = AssignReport {
        centroids: centroids.topics.len(),
        ..AssignReport::default()
    };
    if centroids.topics.is_empty() {
        return Ok(report);
    }
    for memory in memories.all_live().map_err(|e| e.to_string())? {
        if placed.contains(&memory.id) || !is_seedable(&memory) {
            continue;
        }
        report.examined += 1;
        if !assign_memory(store, &memory.id, &centroids, floor)?.is_empty() {
            report.assigned += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::NewMemory;
    use crate::db::Database;

    fn unit(xs: &[f32]) -> Vec<f32> {
        normalize(xs.to_vec())
    }

    fn centroids(list: &[(&str, &[f32])]) -> Centroids {
        Centroids {
            dim: list[0].1.len(),
            topics: list
                .iter()
                .map(|(t, v)| Centroid {
                    topic: t.to_string(),
                    vector: unit(v),
                })
                .collect(),
        }
    }

    #[test]
    fn choose_takes_the_best_and_near_ties_above_the_floor() {
        let c = centroids(&[
            ("topic:A", &[1.0, 0.0, 0.0]),
            ("topic:B", &[0.95, 0.31, 0.0]),
            ("topic:C", &[0.0, 0.0, 1.0]),
        ]);
        assert_eq!(
            choose(&c, &unit(&[1.0, 0.1, 0.0]), 0.5),
            ["topic:A", "topic:B"]
        );
        assert_eq!(choose(&c, &unit(&[0.0, 0.0, 1.0]), 0.5), ["topic:C"]);
        assert!(
            choose(&c, &unit(&[0.0, 1.0, 0.0]), 0.5).is_empty(),
            "below floor"
        );
        assert!(choose(&c, &[1.0, 0.0], 0.5).is_empty(), "wrong dimension");
        assert!(choose(&Centroids::default(), &[], 0.1).is_empty());
    }

    #[test]
    fn choose_caps_the_number_of_topics() {
        let same: &[f32] = &[1.0, 0.0];
        let c = centroids(&[
            ("topic:A", same),
            ("topic:B", same),
            ("topic:C", same),
            ("topic:D", same),
        ]);
        assert_eq!(choose(&c, &unit(same), 0.5).len(), MAX_TOPICS);
    }

    fn insert(store: &Store<'_>, category: &str, v: &[f32]) -> String {
        let id = format!("mem_{}", uuid::Uuid::new_v4().simple());
        let mut row = NewMemory::new(id.clone(), "text", &chrono::Utc::now().to_rfc3339());
        row.category = category.into();
        Memories::new(store).insert(&row).unwrap();
        let bytes: Vec<u8> = unit(v).iter().flat_map(|x| x.to_le_bytes()).collect();
        Vectors::new(store).put(&id, 0, &bytes).unwrap();
        id
    }

    fn make_topic(store: &Store<'_>, label: &str, parent: Parent) {
        topic::set(
            store,
            &topic::SetInput {
                topic: TopicName::parse(label).unwrap(),
                parent: Some(parent),
                status: Some(Status::Active),
                evidence: Vec::new(),
                because: Some("test".into()),
                confirmed_by: "human".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn centroids_cover_leaves_and_a_sweep_places_new_memories() {
        let dir = std::env::temp_dir().join(format!(
            "rmm_topic_assign_{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(dir.join("memory.db")).unwrap();
        let store = db.store();
        make_topic(&store, "Engineering", Parent::Root);
        let eng = Parent::Topic(TopicName::parse("Engineering").unwrap());
        make_topic(&store, "Rust", eng.clone());
        make_topic(&store, "Cooking", Parent::Root);
        let rust = insert(&store, "fact", &[1.0, 0.0]);
        let food = insert(&store, "fact", &[0.0, 1.0]);
        topic::assign(&store, &rust, &TopicName::parse("Rust").unwrap()).unwrap();
        topic::assign(&store, &food, &TopicName::parse("Cooking").unwrap()).unwrap();

        assert!(load(&store).unwrap().is_none());
        let built = rebuild(&store).unwrap();
        let mut leaves: Vec<&str> = built.topics.iter().map(|c| c.topic.as_str()).collect();
        leaves.sort_unstable();
        assert_eq!(
            leaves,
            ["topic:Cooking", "topic:Rust"],
            "Engineering is not a leaf"
        );
        assert_eq!(load(&store).unwrap().unwrap(), built);

        let new = insert(&store, "fact", &[0.9, 0.1]);
        let dialog = insert(&store, crate::models::DIALOG_CATEGORY, &[1.0, 0.0]);
        let far = insert(&store, "fact", &[-1.0, -1.0]);
        let report = assign_pending(&store).unwrap();
        assert_eq!(
            (report.centroids, report.examined, report.assigned),
            (2, 2, 1)
        );
        let rust_members = Memories::new(&store)
            .linked_ids(&TopicName::parse("Rust").unwrap().entity_id())
            .unwrap();
        assert!(rust_members.contains(&new));
        assert!(!rust_members.contains(&dialog), "dialogs are not assigned");
        assert!(!rust_members.contains(&far));

        let later = insert(&store, "fact", &[0.0, 1.0]);
        assign_on_write(&store, &later);
        let cooking = Memories::new(&store)
            .linked_ids(&TopicName::parse("Cooking").unwrap().entity_id())
            .unwrap();
        assert!(
            cooking.contains(&later),
            "the write hook uses the saved centroids"
        );
        drop(store);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
