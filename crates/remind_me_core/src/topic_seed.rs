//! Seeding the topic taxonomy: cluster, name, apply
//! (`docs/design/topic-taxonomy.md` §2).
//!
//! Three steps, each resumable, and nothing reaches the store until a person
//! runs [`apply`]:
//!
//! 1. [`seed`] clusters one vector per refined memory with recursive
//!    spherical k-means and writes a [`Snapshot`] file beside the store.
//! 2. [`name_pending`] asks a [`TopicNamer`] to name each node, leaves
//!    first, saving the snapshot after every name, so a stopped run resumes
//!    at the first unnamed node.
//! 3. [`apply`] writes the named tree through [`crate::topic`]: one topic per
//!    node and one membership link per leaf member. Running it again changes
//!    nothing.
//!
//! The clustering is pure and deterministic (farthest-first initialisation,
//! no randomness), so a run is reproducible and testable on hand-made
//! vectors.

use crate::db::memories::Memories;
use crate::db::vectors::Vectors;
use crate::db::{Store, StoreError};
use crate::models::{Memory, DIALOG_CATEGORY, SKELETON_CATEGORY};
use crate::topic::{self, Parent, Status, TopicName, MAX_LABEL_CHARS};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// How a seed run shapes the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedParams {
    /// Clusters at the top level.
    pub top_k: usize,
    /// A node larger than this is split, while depth allows.
    pub leaf_max: usize,
    /// Members per child a split aims for.
    pub split_per: usize,
    /// Deepest level, the top being 1.
    pub max_depth: usize,
    /// Members nearest a node's centroid kept to name it.
    pub exemplars: usize,
    /// Lloyd iterations per split, at most.
    pub iterations: usize,
}

impl Default for SeedParams {
    fn default() -> Self {
        Self {
            top_k: 12,
            leaf_max: 300,
            split_per: 150,
            max_depth: 3,
            exemplars: 12,
            iterations: 25,
        }
    }
}

/// One node of a seeded tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeedNode {
    pub id: usize,
    pub parent: Option<usize>,
    pub depth: usize,
    /// Memories in this node and every node below it.
    pub size: usize,
    /// Members, on a leaf only; an interior node's members are its leaves'.
    pub members: Vec<String>,
    /// Members nearest the centroid, most central first.
    pub exemplars: Vec<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

impl SeedNode {
    pub fn is_leaf(&self) -> bool {
        !self.members.is_empty()
    }
}

/// The embedding the vectors came from, as recorded in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedEmbedding {
    pub backend: String,
    pub model: String,
    pub dim: usize,
}

/// A seed run, written to `<store>.topics/seed-<run>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub run: String,
    pub created_at: String,
    pub embedding: Option<SeedEmbedding>,
    pub params: SeedParams,
    /// Breadth first: a parent's id is always below its children's.
    pub nodes: Vec<SeedNode>,
}

impl Snapshot {
    /// Nodes still without a label.
    pub fn unnamed(&self) -> usize {
        self.nodes.iter().filter(|n| n.label.is_none()).count()
    }

    fn children(&self, id: usize) -> impl Iterator<Item = &SeedNode> {
        self.nodes.iter().filter(move |n| n.parent == Some(id))
    }
}

// --- step 1: cluster -------------------------------------------------------

/// Cluster `points` (unit vectors, all one dimension) into a tree of nodes
/// over their indices. Pure and deterministic.
pub fn cluster_tree(points: &[Vec<f32>], params: &SeedParams) -> Vec<ClusterNode> {
    let mut nodes = vec![ClusterNode {
        parent: None,
        depth: 0,
        members: (0..points.len()).collect(),
        centroid: centroid(points, &(0..points.len()).collect::<Vec<_>>()),
    }];
    let mut next = 0;
    while next < nodes.len() {
        let node = &nodes[next];
        let k = split_k(node, params);
        // The root always gets children, even just one: every topic is a
        // node below it. Any other node splits only into two or more.
        let floor = if node.depth == 0 { 1 } else { 2 };
        if k >= floor {
            let parts = kmeans(points, &node.members, k, params.iterations);
            if parts.len() >= floor {
                let depth = node.depth + 1;
                for (members, centroid) in parts {
                    nodes.push(ClusterNode {
                        parent: Some(next),
                        depth,
                        members,
                        centroid,
                    });
                }
            }
        }
        next += 1;
    }
    nodes
}

/// One node of [`cluster_tree`]. Node 0 is the whole set (the root); every
/// other node is a topic.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterNode {
    pub parent: Option<usize>,
    /// 0 for the root, 1 for a top-level topic.
    pub depth: usize,
    /// Indices into the points, this node and everything below it.
    pub members: Vec<usize>,
    pub centroid: Vec<f32>,
}

/// How many children to split `node` into; below 2 means keep it whole.
fn split_k(node: &ClusterNode, params: &SeedParams) -> usize {
    let n = node.members.len();
    if node.depth == 0 {
        return params.top_k.min(n / 2).max(usize::from(n > 0));
    }
    if node.depth >= params.max_depth || n <= params.leaf_max {
        return 0;
    }
    (n / params.split_per.max(1)).clamp(2, 8)
}

/// Split `members` into at most `k` non-empty clusters by spherical k-means,
/// each with its unit centroid. Fewer when points coincide.
fn kmeans(
    points: &[Vec<f32>],
    members: &[usize],
    k: usize,
    iterations: usize,
) -> Vec<(Vec<usize>, Vec<f32>)> {
    let mut centers = farthest_first(points, members, k);
    let mut assignment: Vec<usize> = vec![usize::MAX; members.len()];
    for _ in 0..iterations.max(1) {
        let mut changed = false;
        for (slot, &p) in assignment.iter_mut().zip(members) {
            let best = nearest(&points[p], &centers);
            changed |= *slot != best;
            *slot = best;
        }
        centers = (0..centers.len())
            .map(|c| {
                let own: Vec<usize> = members
                    .iter()
                    .zip(&assignment)
                    .filter(|(_, &a)| a == c)
                    .map(|(&p, _)| p)
                    .collect();
                if own.is_empty() {
                    centers[c].clone()
                } else {
                    centroid(points, &own)
                }
            })
            .collect();
        if !changed {
            break;
        }
    }
    (0..centers.len())
        .map(|c| {
            let own: Vec<usize> = members
                .iter()
                .zip(&assignment)
                .filter(|(_, &a)| a == c)
                .map(|(&p, _)| p)
                .collect();
            (own, centers[c].clone())
        })
        .filter(|(own, _)| !own.is_empty())
        .collect()
}

/// Deterministic seeds: the member closest to the mean, then repeatedly the
/// member least similar to every seed so far. Stops early when the rest
/// coincide with a seed.
fn farthest_first(points: &[Vec<f32>], members: &[usize], k: usize) -> Vec<Vec<f32>> {
    let Some(&first) = members.first() else {
        return Vec::new();
    };
    let mean = centroid(points, members);
    let start = members
        .iter()
        .copied()
        .max_by(|&a, &b| dot(&points[a], &mean).total_cmp(&dot(&points[b], &mean)))
        .unwrap_or(first);
    let mut centers = vec![points[start].clone()];
    let mut closest: Vec<f32> = members
        .iter()
        .map(|&p| dot(&points[p], &centers[0]))
        .collect();
    while centers.len() < k {
        let (at, &sim) = closest
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .unwrap_or((0, &1.0));
        if sim >= 1.0 - 1e-6 {
            break;
        }
        let pick = points[members[at]].clone();
        for (c, &p) in closest.iter_mut().zip(members) {
            *c = c.max(dot(&points[p], &pick));
        }
        centers.push(pick);
    }
    centers
}

fn nearest(point: &[f32], centers: &[Vec<f32>]) -> usize {
    centers
        .iter()
        .enumerate()
        .max_by(|a, b| dot(point, a.1).total_cmp(&dot(point, b.1)))
        .map_or(0, |(i, _)| i)
}

/// The unit mean of `members`, or the zero vector when it has no length.
fn centroid(points: &[Vec<f32>], members: &[usize]) -> Vec<f32> {
    let dim = members.first().map_or(0, |&p| points[p].len());
    let mut sum = vec![0.0f32; dim];
    for &p in members {
        for (s, x) in sum.iter_mut().zip(&points[p]) {
            *s += x;
        }
    }
    normalize(sum)
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Whether a memory is knowledge a topic should hold: not a raw dialog or
/// skeleton (they match everything and inherit topics from what was promoted
/// out of them), not sensitive, and not a project or topic record.
pub fn is_seedable(memory: &Memory) -> bool {
    memory.category != DIALOG_CATEGORY
        && memory.category != SKELETON_CATEGORY
        && !memory.sensitive
        && !memory
            .predicate
            .as_deref()
            .is_some_and(crate::project::is_reserved_predicate)
        && memory.source != "topic"
        && memory.source != "project"
}

/// One unit vector per seedable memory with vectors: the normalised mean of
/// its chunks. Memories whose chunks disagree on dimension are skipped.
pub fn memory_vectors(store: &Store<'_>) -> Result<Vec<(String, Vec<f32>)>, StoreError> {
    let ids: Vec<String> = Memories::new(store)
        .all_live()?
        .into_iter()
        .filter(is_seedable)
        .map(|m| m.id)
        .collect();
    let mut sums: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let mut bad: HashSet<String> = HashSet::new();
    for chunk in Vectors::new(store).live_chunks(None, Some(&ids))? {
        let v = crate::vectors::le_bytes_to_f32(&chunk.embedding);
        let sum = sums
            .entry(chunk.memory_id.clone())
            .or_insert_with(|| vec![0.0; v.len()]);
        if sum.len() != v.len() {
            bad.insert(chunk.memory_id);
            continue;
        }
        sum.iter_mut().zip(&v).for_each(|(s, x)| *s += x);
    }
    let dim = sums.values().map(Vec::len).max().unwrap_or(0);
    Ok(sums
        .into_iter()
        .filter(|(id, v)| !bad.contains(id) && v.len() == dim && dim > 0)
        .map(|(id, v)| (id, normalize(v)))
        .collect())
}

/// Cluster every seedable memory and save the run. Errors when nothing is
/// embedded or the store is in memory.
pub fn seed(store: &Store<'_>, params: &SeedParams) -> Result<Snapshot, String> {
    let dir = snapshot_dir(store)?;
    let vectors = memory_vectors(store).map_err(|e| e.to_string())?;
    if vectors.len() < 2 {
        return Err(format!(
            "seeding needs at least 2 embedded memories, found {}; run remind_me_reindex first",
            vectors.len()
        ));
    }
    let (ids, points): (Vec<String>, Vec<Vec<f32>>) = vectors.into_iter().unzip();
    let tree = cluster_tree(&points, params);
    let now = chrono::Utc::now();
    let snapshot = Snapshot {
        run: now.format("%Y%m%dT%H%M%SZ").to_string(),
        created_at: now.to_rfc3339(),
        embedding: crate::vectors::read_embedding_meta(store)
            .map_err(|e| e.to_string())?
            .map(|e| SeedEmbedding {
                backend: e.backend,
                model: e.model,
                dim: e.dim,
            }),
        params: *params,
        nodes: snapshot_nodes(&tree, &ids, &points, params.exemplars),
    };
    save(&dir, &snapshot)?;
    Ok(snapshot)
}

/// The topic nodes of `tree` (its root dropped), with member ids.
fn snapshot_nodes(
    tree: &[ClusterNode],
    ids: &[String],
    points: &[Vec<f32>],
    exemplars: usize,
) -> Vec<SeedNode> {
    // Tree index -> snapshot id; the root (index 0) has none.
    let mut index: HashMap<usize, usize> = HashMap::new();
    let mut out = Vec::new();
    for (i, node) in tree.iter().enumerate().skip(1) {
        let id = out.len();
        index.insert(i, id);
        let is_leaf = !tree.iter().any(|n| n.parent == Some(i));
        let mut ranked = node.members.clone();
        ranked.sort_by(|&a, &b| {
            dot(&points[b], &node.centroid).total_cmp(&dot(&points[a], &node.centroid))
        });
        out.push(SeedNode {
            id,
            parent: node.parent.and_then(|p| index.get(&p).copied()),
            depth: node.depth,
            size: node.members.len(),
            members: if is_leaf {
                node.members.iter().map(|&p| ids[p].clone()).collect()
            } else {
                Vec::new()
            },
            exemplars: ranked
                .into_iter()
                .take(exemplars)
                .map(|p| ids[p].clone())
                .collect(),
            label: None,
            description: None,
        });
    }
    out
}

// --- snapshot files --------------------------------------------------------

/// `<store>.topics/`, where seed runs are kept.
pub fn snapshot_dir(store: &Store<'_>) -> Result<PathBuf, String> {
    let db = store
        .path()
        .ok_or("an in-memory store has nowhere to keep a seed run")?;
    Ok(PathBuf::from(format!("{}.topics", db.display())))
}

fn snapshot_path(dir: &Path, run: &str) -> Result<PathBuf, String> {
    let valid = !run.is_empty() && run.chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
        return Err(format!("'{run}' is not a seed run id"));
    }
    Ok(dir.join(format!("seed-{run}.json")))
}

/// Write `snapshot` atomically: a temporary file, then a rename.
pub fn save(dir: &Path, snapshot: &Snapshot) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = snapshot_path(dir, &snapshot.run)?;
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(snapshot).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Read seed run `run` from `dir`.
pub fn load(dir: &Path, run: &str) -> Result<Snapshot, String> {
    let path = snapshot_path(dir, run)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not a seed run: {e}", path.display()))
}

// --- step 2: name ----------------------------------------------------------

/// What a namer is shown for one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameRequest {
    /// Names already given to the node's children (empty for a leaf).
    pub children: Vec<String>,
    /// The most central members' text, cut short.
    pub samples: Vec<String>,
}

/// A namer's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedTopic {
    pub name: String,
    pub description: String,
}

/// Names a topic. The port the local model sits behind.
pub trait TopicNamer {
    fn name_topic(&self, request: &NameRequest) -> Result<NamedTopic, String>;
}

/// Characters of each sample a namer sees.
pub const SAMPLE_CHARS: usize = 300;

/// Reads the text of memories by id, for a namer's samples.
pub type SampleReader<'a> = dyn Fn(&[String]) -> Result<Vec<String>, String> + 'a;

/// What [`name_pending`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameReport {
    pub run: String,
    pub named: usize,
    pub remaining: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

/// Name up to `max_nodes` unnamed nodes of run `run`, deepest first so a
/// parent sees its children's names. `samples` reads a node's exemplar text;
/// it is called for each node, so it can hold the store only briefly. The
/// snapshot is saved after every name.
pub fn name_pending(
    dir: &Path,
    run: &str,
    namer: &dyn TopicNamer,
    samples: &SampleReader<'_>,
    max_nodes: usize,
) -> Result<NameReport, String> {
    let mut snapshot = load(dir, run)?;
    let mut report = NameReport {
        run: run.to_string(),
        ..NameReport::default()
    };
    let mut order: Vec<usize> = (0..snapshot.nodes.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse((snapshot.nodes[i].depth, i)));
    for i in order {
        if report.named >= max_nodes {
            break;
        }
        if snapshot.nodes[i].label.is_some() {
            continue;
        }
        let children: Vec<String> = snapshot
            .children(i)
            .filter_map(|c| c.label.clone())
            .collect();
        let texts = samples(&snapshot.nodes[i].exemplars)?;
        let request = NameRequest {
            children,
            samples: texts.iter().map(|t| cut(t, SAMPLE_CHARS)).collect(),
        };
        match namer.name_topic(&request) {
            Ok(named) => {
                let node = &mut snapshot.nodes[i];
                node.label = Some(clean_label(&named.name));
                node.description = Some(named.description.trim().to_string());
                save(dir, &snapshot)?;
                report.named += 1;
            }
            Err(why) => {
                report.stopped = Some(why);
                break;
            }
        }
    }
    report.remaining = snapshot.unnamed();
    Ok(report)
}

fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// A model's name as a topic label: trimmed of quotes and a `topic:`
/// prefix, whitespace collapsed, at most [`MAX_LABEL_CHARS`].
fn clean_label(name: &str) -> String {
    let trimmed = name
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`');
    let trimmed = trimmed
        .get(..topic::ENTITY_PREFIX.len())
        .filter(|head| head.eq_ignore_ascii_case(topic::ENTITY_PREFIX))
        .map_or(trimmed, |_| &trimmed[topic::ENTITY_PREFIX.len()..]);
    let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&collapsed, MAX_LABEL_CHARS)
}

// --- step 3: apply ---------------------------------------------------------

/// One topic [`apply`] writes or would write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTopic {
    pub topic: String,
    pub parent: String,
    pub size: usize,
    pub depth: usize,
}

/// What [`apply`] did, or would do on a dry run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub run: String,
    pub dry_run: bool,
    pub plan: Vec<PlannedTopic>,
    pub topics_written: usize,
    pub links_added: usize,
    /// Members deleted or changed since the run, left out.
    pub members_skipped: usize,
}

/// Write the named tree of run `run`: each node a topic with its parent and
/// status active, each leaf member linked. Refused while any node is
/// unnamed. A label used twice is qualified with its parent's label.
pub fn apply(
    store: &Store<'_>,
    run: &str,
    confirmed_by: &str,
    dry_run: bool,
) -> Result<ApplyReport, String> {
    let snapshot = load(&snapshot_dir(store)?, run)?;
    let unnamed = snapshot.unnamed();
    if unnamed > 0 {
        return Err(format!(
            "{unnamed} node(s) of run {run} have no name yet; run topics name first"
        ));
    }
    let names = unique_names(&snapshot)?;
    let mut report = ApplyReport {
        run: run.to_string(),
        dry_run,
        ..ApplyReport::default()
    };
    for node in &snapshot.nodes {
        let parent = node
            .parent
            .map_or(Parent::Root, |p| Parent::Topic(names[p].clone()));
        report.plan.push(PlannedTopic {
            topic: names[node.id].as_str().to_string(),
            parent: match &parent {
                Parent::Root => topic::ROOT.to_string(),
                Parent::Topic(p) => p.as_str().to_string(),
            },
            size: node.size,
            depth: node.depth,
        });
        if dry_run {
            continue;
        }
        let evidence = live_ids(store, &node.exemplars)?;
        let because = evidence
            .is_empty()
            .then(|| format!("Seeded by topics run {run}."));
        let outcome = topic::set(
            store,
            &topic::SetInput {
                topic: names[node.id].clone(),
                parent: Some(parent),
                status: Some(Status::Active),
                evidence,
                because,
                confirmed_by: confirmed_by.to_string(),
            },
        )
        .map_err(|e| format!("topic {}: {e}", names[node.id]))?;
        report.topics_written += usize::from(!outcome.written.is_empty());
        for member in &node.members {
            match topic::assign(store, member, &names[node.id]) {
                Ok(new) => report.links_added += usize::from(new),
                Err(StoreError::NotFound) => report.members_skipped += 1,
                Err(StoreError::Invalid(_)) => report.members_skipped += 1,
                Err(e) => return Err(format!("linking {member}: {e}")),
            }
        }
    }
    Ok(report)
}

/// Each node's topic name, unique by normalised label: a repeat takes its
/// parent's label in brackets, then a number.
fn unique_names(snapshot: &Snapshot) -> Result<Vec<TopicName>, String> {
    let mut taken: HashSet<String> = HashSet::new();
    let mut names: Vec<TopicName> = Vec::with_capacity(snapshot.nodes.len());
    for node in &snapshot.nodes {
        let label = node.label.as_deref().unwrap_or_default();
        let parent = node
            .parent
            .map(|p| names[p].label().to_string())
            .unwrap_or_default();
        let candidates = std::iter::once(label.to_string())
            .chain((!parent.is_empty()).then(|| format!("{label} ({parent})")))
            .chain((2..).map(|n| format!("{label} {n}")));
        let name = candidates
            .take(100)
            .filter_map(|c| TopicName::parse(&c).ok())
            .find(|n| !taken.contains(&crate::entity::normalize_entity_name(n.as_str())))
            .ok_or_else(|| format!("node {} has no usable label ({label:?})", node.id))?;
        taken.insert(crate::entity::normalize_entity_name(name.as_str()));
        names.push(name);
    }
    Ok(names)
}

fn live_ids(store: &Store<'_>, ids: &[String]) -> Result<Vec<String>, String> {
    let memories = Memories::new(store);
    let mut out = Vec::new();
    for id in ids {
        if memories.get_live(id).map_err(|e| e.to_string())?.is_some() {
            out.push(id.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::NewMemory;
    use crate::db::Database;
    use std::cell::Cell;

    /// A fresh directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rmm_topic_seed_{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A unit vector along `axis`, nudged by `jitter` on the next axis.
    fn v(axis: usize, jitter: f32) -> Vec<f32> {
        let mut out = vec![0.0; 6];
        out[axis] = 1.0;
        out[(axis + 1) % 6] = jitter;
        normalize(out)
    }

    fn three_groups(each: usize) -> Vec<Vec<f32>> {
        (0..each * 3)
            .map(|i| v((i % 3) * 2, (i / 3) as f32 * 0.01))
            .collect()
    }

    #[test]
    fn separated_groups_come_back_whole_and_the_run_repeats() {
        let points = three_groups(10);
        let params = SeedParams {
            top_k: 3,
            ..SeedParams::default()
        };
        let tree = cluster_tree(&points, &params);
        assert_eq!(tree.len(), 4, "root and three topics");
        for node in &tree[1..] {
            let axis = |p: usize| (p % 3) * 2;
            let first = axis(node.members[0]);
            assert!(node.members.iter().all(|&p| axis(p) == first), "pure");
            assert_eq!(node.members.len(), 10);
        }
        assert_eq!(cluster_tree(&points, &params), tree, "deterministic");
    }

    #[test]
    fn a_big_node_splits_until_depth_runs_out() {
        let points = three_groups(40);
        let params = SeedParams {
            top_k: 1,
            leaf_max: 30,
            split_per: 40,
            max_depth: 2,
            ..SeedParams::default()
        };
        let tree = cluster_tree(&points, &params);
        // Root -> 1 top topic of 120 -> 3 children (120/40); depth 2 stops.
        let top: Vec<_> = tree.iter().filter(|n| n.depth == 1).collect();
        let second: Vec<_> = tree.iter().filter(|n| n.depth == 2).collect();
        assert_eq!(top.len(), 1);
        assert_eq!(second.len(), 3);
        assert!(tree.iter().all(|n| n.depth <= 2));
    }

    #[test]
    fn identical_points_do_not_split_into_empty_clusters() {
        let points = vec![v(0, 0.0); 8];
        let tree = cluster_tree(&points, &SeedParams::default());
        assert!(tree.iter().all(|n| !n.members.is_empty()));
        assert_eq!(tree.len(), 2, "root and one topic");
    }

    #[test]
    fn labels_are_cleaned_and_repeats_are_qualified() {
        assert_eq!(
            clean_label("  \"Topic:Systems   Engineering\" "),
            "Systems Engineering"
        );
        let node = |id, parent, label: &str| SeedNode {
            id,
            parent,
            depth: if parent.is_some() { 2 } else { 1 },
            size: 1,
            members: Vec::new(),
            exemplars: Vec::new(),
            label: Some(label.into()),
            description: None,
        };
        let snapshot = Snapshot {
            run: "r1".into(),
            created_at: String::new(),
            embedding: None,
            params: SeedParams::default(),
            nodes: vec![
                node(0, None, "Hardware"),
                node(1, None, "Software"),
                node(2, Some(0), "Testing"),
                node(3, Some(1), "testing"),
            ],
        };
        let names: Vec<String> = unique_names(&snapshot)
            .unwrap()
            .iter()
            .map(|n| n.label().to_string())
            .collect();
        assert_eq!(
            names,
            ["Hardware", "Software", "Testing", "testing (Software)"]
        );
    }

    struct Counting {
        calls: Cell<usize>,
        fail_at: usize,
    }

    impl TopicNamer for Counting {
        fn name_topic(&self, request: &NameRequest) -> Result<NamedTopic, String> {
            let n = self.calls.get() + 1;
            self.calls.set(n);
            if n == self.fail_at {
                return Err("model down".into());
            }
            Ok(NamedTopic {
                name: format!("T{n} {}", request.children.len()),
                description: "d".into(),
            })
        }
    }

    fn fixture(dir: &Path) -> Snapshot {
        let node = |id, parent: Option<usize>, depth| SeedNode {
            id,
            parent,
            depth,
            size: 1,
            members: Vec::new(),
            exemplars: vec![format!("mem_{id}")],
            label: None,
            description: None,
        };
        let snapshot = Snapshot {
            run: "r2".into(),
            created_at: String::new(),
            embedding: None,
            params: SeedParams::default(),
            nodes: vec![node(0, None, 1), node(1, Some(0), 2), node(2, Some(0), 2)],
        };
        save(dir, &snapshot).unwrap();
        snapshot
    }

    #[test]
    fn naming_goes_leaves_first_and_resumes_after_a_failure() {
        let dir = TempDir::new();
        fixture(dir.path());
        let samples = |ids: &[String]| Ok(ids.to_vec());
        let flaky = Counting {
            calls: Cell::new(0),
            fail_at: 2,
        };
        let first = name_pending(dir.path(), "r2", &flaky, &samples, 10).unwrap();
        assert_eq!((first.named, first.remaining), (1, 2));
        assert!(first.stopped.is_some());

        let steady = Counting {
            calls: Cell::new(0),
            fail_at: 0,
        };
        let second = name_pending(dir.path(), "r2", &steady, &samples, 10).unwrap();
        assert_eq!((second.named, second.remaining), (2, 0));
        let saved = load(dir.path(), "r2").unwrap();
        // The parent was named last and saw both children's names.
        assert!(saved.nodes[0].label.as_deref().unwrap().ends_with(" 2"));
    }

    #[test]
    fn run_ids_cannot_escape_the_directory() {
        let dir = TempDir::new();
        assert!(load(dir.path(), "../x").is_err());
        assert!(load(dir.path(), "").is_err());
    }

    fn insert(store: &Store<'_>, category: &str, embedding: &[f32]) -> String {
        let id = format!("mem_{}", uuid::Uuid::new_v4().simple());
        let now = chrono::Utc::now().to_rfc3339();
        let mut row = NewMemory::new(id.clone(), format!("{category} memory"), &now);
        row.category = category.to_string();
        Memories::new(store).insert(&row).unwrap();
        let bytes: Vec<u8> = embedding.iter().flat_map(|x| x.to_le_bytes()).collect();
        Vectors::new(store).put(&id, 0, &bytes).unwrap();
        id
    }

    #[test]
    fn seed_name_apply_end_to_end() {
        let dir = TempDir::new();
        let db = Database::open(dir.path().join("memory.db")).unwrap();
        let store = db.store();
        let mut facts = Vec::new();
        for i in 0..6 {
            facts.push(insert(&store, "fact", &v((i % 2) * 3, i as f32 * 0.01)));
        }
        let dialog = insert(&store, DIALOG_CATEGORY, &v(0, 0.0));

        let params = SeedParams {
            top_k: 2,
            ..SeedParams::default()
        };
        let snapshot = seed(&store, &params).unwrap();
        assert_eq!(snapshot.nodes.len(), 2);
        let all: Vec<&String> = snapshot.nodes.iter().flat_map(|n| &n.members).collect();
        assert_eq!(all.len(), 6);
        assert!(!all.contains(&&dialog), "dialogs are not seeded");

        let tree_dir = snapshot_dir(&store).unwrap();
        assert!(
            apply(&store, &snapshot.run, "human", false).is_err(),
            "unnamed"
        );
        let namer = Counting {
            calls: Cell::new(0),
            fail_at: 0,
        };
        let samples = |ids: &[String]| Ok(ids.to_vec());
        name_pending(&tree_dir, &snapshot.run, &namer, &samples, 10).unwrap();

        let dry = apply(&store, &snapshot.run, "human", true).unwrap();
        assert_eq!(dry.plan.len(), 2);
        assert!(
            topic::tree(&store).unwrap().topics.is_empty(),
            "dry run writes nothing"
        );

        let done = apply(&store, &snapshot.run, "human", false).unwrap();
        assert_eq!((done.topics_written, done.links_added), (2, 6));
        let tree = topic::tree(&store).unwrap();
        assert_eq!(tree.topics.iter().map(|t| t.member_count).sum::<usize>(), 6);

        let again = apply(&store, &snapshot.run, "human", false).unwrap();
        assert_eq!(
            (again.topics_written, again.links_added),
            (0, 0),
            "idempotent"
        );
    }
}
