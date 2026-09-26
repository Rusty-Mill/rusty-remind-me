use crate::db::entities::{Entities, RelationRow};
use crate::db::memories::Memories;
use crate::models::EntityInput;
use chrono::Utc;
use rusqlite::{Connection, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub aliases: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Normalise an entity name for identity: lowercased, whitespace collapsed.
///
/// Collapsing *internal* runs matters as much as trimming the ends —
/// `"Bailey  Robertson"` and `"bailey robertson"` name the same person, and the
/// id is derived from this form so they resolve to one row. An earlier version
/// only trimmed, which made them two entities here and one in `remind_me`.
///
/// Shared by every path that needs an entity's identity, so no caller can
/// normalise differently.
pub fn normalize_entity_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The deterministic id for an entity name.
///
/// Content hash, no timestamp: two machines that independently record the same
/// entity converge on the same row rather than conflicting. That only works if
/// both derive the id identically, so this mirrors `remind_me`'s `_entity_id`
/// exactly — sha256 of the normalised name, truncated to 12 hex characters,
/// unprefixed.
///
/// Twelve hex characters is 48 bits. That collision domain is inherited from
/// the reference rather than chosen here; widening it would break interop,
/// which is the whole reason the id is derived at all.
pub fn entity_id(name: &str) -> String {
    sha256::digest(normalize_entity_name(name))[..12].to_string()
}

/// Insert an entity, or merge into the existing row of the same name.
///
/// Aliases **union-merge**: existing first, then new ones, de-duplicated and
/// order-preserving. A missing `kind` is filled in, but an existing `kind` is
/// never overwritten — the reference resolves this the same way (`row["kind"] or
/// kind`), so a later mention that guesses a different kind cannot clobber a
/// deliberate earlier one.
///
/// `updated_at` moves only when something actually changed, so a no-op mention
/// does not churn the row.
pub fn upsert_entity(conn: &Connection, input: &EntityInput) -> Result<Entity> {
    let now = Utc::now().to_rfc3339();
    let name = input.name.trim();
    let id = entity_id(name);

    let clean_aliases: Vec<String> = dedup_preserving_order(
        input
            .aliases
            .iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty()),
    );

    // Key on the derived id, not on `name`. The id is the identity — it is
    // built from the case-folded name precisely so "Tasmania" and "tasmania"
    // are one entity. Matching on the `name` column instead is case-sensitive,
    // so a casing variant misses the lookup, tries to insert, and collides on
    // the `entities.id` unique constraint.
    let entities = Entities::new(conn);
    match entities.get(&id)? {
        None => {
            let entity = Entity {
                id: id.clone(),
                name: name.to_string(),
                kind: input.kind.clone(),
                aliases: clean_aliases,
                created_at: now.clone(),
                updated_at: now,
            };
            entities.insert(&entity, Some(&crate::sync::configured_node_id()))?;
        }
        Some(existing) => {
            let merged = dedup_preserving_order(
                existing
                    .aliases
                    .iter()
                    .cloned()
                    .chain(clean_aliases.clone()),
            );
            // Existing kind wins; `input.kind` only fills a hole.
            let new_kind = existing.kind.clone().or_else(|| input.kind.clone());

            if merged != existing.aliases || new_kind != existing.kind {
                entities.set_kind_and_aliases(&id, new_kind.as_deref(), &merged, &now)?;
            }
        }
    }

    entities
        .get(&id)?
        .ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// Fetch an entity by its deterministic id.
pub fn get_entity_by_id(conn: &Connection, id: &str) -> Result<Option<Entity>> {
    Entities::new(conn).get(id)
}

pub(crate) fn dedup_preserving_order<I: IntoIterator<Item = String>>(items: I) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

/// Record that a memory mentions an entity. Returns `true` if the link is new.
///
/// Insert-or-ignore: mention links are immutable, and re-annotating with the
/// same entity is a no-op rather than an error.
pub fn link_memory_entity(conn: &Connection, memory_id: &str, entity_id: &str) -> Result<bool> {
    Entities::new(conn).link(memory_id, entity_id, &Utc::now().to_rfc3339())
}

/// Upsert each mentioned entity and link it to `memory_id`.
///
/// Returns the number of **new** links created; entities already linked to this
/// memory are counted as zero. Shared by `add_memory` and `remind_me_annotate`
/// so both apply mentions identically.
pub fn apply_entity_mentions(
    conn: &Connection,
    memory_id: &str,
    entities: &[EntityInput],
) -> Result<usize> {
    let mut linked = 0;
    for input in entities {
        if input.name.trim().is_empty() {
            continue;
        }
        let entity = upsert_entity(conn, input)?;
        if link_memory_entity(conn, memory_id, &entity.id)? {
            linked += 1;
        }
    }
    Ok(linked)
}

/// Fetch an entity by name, case- and whitespace-insensitively.
///
/// Resolves through the derived id rather than matching the `name` column, so
/// `"tasmania"` finds the entity stored as `"Tasmania"` — the same identity the
/// id encodes.
pub fn get_entity_by_name(conn: &Connection, name: &str) -> Result<Option<Entity>> {
    get_entity_by_id(conn, &entity_id(name))
}

/// Resolve a name *or alias* to its canonical entity row.
///
/// Two stages, mirroring the reference:
///
/// 1. **Derived-id lookup.** An indexed primary-key hit whenever the query is
///    the entity's canonical name, in any casing or spacing.
/// 2. **Fallback scan.** A canonical-name match first — defensive, since ids are
///    derived from names, so this only fires for a row whose stored name has
///    drifted from its id — then a match against the `aliases` array.
///
/// A canonical-name match anywhere in the scan beats an alias match found
/// earlier, because an alias is a nickname and the canonical name is the thing
/// itself. That is why the alias hit is held rather than returned immediately.
pub fn resolve_entity(conn: &Connection, query: &str) -> Result<Option<Entity>> {
    if let Some(entity) = get_entity_by_id(conn, &entity_id(query))? {
        return Ok(Some(entity));
    }

    let normalized = normalize_entity_name(query);
    if normalized.is_empty() {
        return Ok(None);
    }

    let mut alias_hit: Option<Entity> = None;
    for entity in Entities::new(conn).all()? {
        if normalize_entity_name(&entity.name) == normalized {
            return Ok(Some(entity));
        }
        if alias_hit.is_none()
            && entity
                .aliases
                .iter()
                .any(|a| normalize_entity_name(a) == normalized)
        {
            alias_hit = Some(entity);
        }
    }
    Ok(alias_hit)
}

/// A memory whose subject or object equals an entity's canonical name.
///
/// SPO fields are written verbatim by the caller (part 1 of annotation), so
/// the match against the canonical name is case-insensitive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityFact {
    pub id: String,
    pub content: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub category: String,
    pub created_at: String,
}

/// A memory linked to an entity via `memory_entities`, trimmed for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityLinkedMemory {
    pub id: String,
    /// First 300 characters of the content, matching the reference.
    pub content_snippet: String,
    pub category: String,
    pub created_at: String,
}

/// The full lookup payload for one entity: its row, its facts, and the
/// memories that mention it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityProfile {
    pub entity: Entity,
    pub facts: Vec<EntityFact>,
    pub memories: Vec<EntityLinkedMemory>,
    /// Every memory linked to this entity, not just the page in `memories` —
    /// so a caller can tell whether raising `limit` would surface more.
    pub total_linked_memories: usize,
}

/// Build the full lookup payload for an entity: row + facts + memories.
///
/// Shared by the `remind_me_entity` MCP tool's lookup path and `GET
/// /api/entity`, so a dashboard and an LLM client see identical data.
///
/// Facts are non-superseded, non-deleted memories whose SPO subject or object
/// equals the entity's canonical name. Linked memories come from
/// `memory_entities` via an inner join, so a dangling link — one delivered by
/// sync before the memory it points at — is invisible rather than a null-row
/// crash. Superseded and deleted memories are excluded from both (`DI-02`).
///
/// Returns `None` when the entity is unknown, so a caller can answer with 404
/// rather than an empty-but-200 profile.
pub fn entity_profile(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Option<EntityProfile>> {
    let Some(entity) = resolve_entity(conn, query)? else {
        return Ok(None);
    };

    let canonical = normalize_entity_name(&entity.name);
    let entities = Entities::new(conn);
    let facts = entities.facts_naming(&canonical, limit)?;
    let memories = entities.linked_memories(&entity.id, limit)?;
    let total_linked_memories = entities.linked_memory_count(&entity.id)?;

    Ok(Some(EntityProfile {
        entity,
        facts,
        memories,
        total_linked_memories,
    }))
}

/// One row of [`list_entities`], with its mention count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityListItem {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub aliases: Vec<String>,
    pub updated_at: String,
    /// Linked-memory count via `memory_entities`.
    pub mention_count: i64,
}

/// A page of [`list_entities`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityListResult {
    pub total: usize,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub entities: Vec<EntityListItem>,
}

/// List entities, most-mentioned first.
///
/// There is no MCP-tool equivalent — `remind_me_entity` is lookup-by-name, and
/// browsing everything by list is specifically a dashboard need — so this is
/// used only by `GET /api/entities`.
pub fn list_entities(conn: &Connection, limit: usize, offset: usize) -> Result<EntityListResult> {
    let repo = Entities::new(conn);
    let total = repo.count()?;
    let entities = repo.page_by_mentions(limit, offset)?;

    let count = entities.len();
    Ok(EntityListResult {
        total,
        count,
        offset,
        limit,
        has_more: total > offset + count,
        entities,
    })
}

/// Maximum relation edges a traversal returns, across all hops.
pub const RELATION_TRAVERSAL_CAP: usize = 20;
/// Bounds on `hops`, matching the reference's `EntityTraverseInput`.
pub const TRAVERSE_HOPS_MIN: u32 = 1;
pub const TRAVERSE_HOPS_MAX: u32 = 3;

/// One typed edge of the entity-relation graph, tagged with the hop that found
/// it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationEdge {
    pub subject_entity_id: String,
    pub subject_name: String,
    pub subject_kind: Option<String>,
    pub relation: String,
    pub object_entity_id: String,
    pub object_name: String,
    pub object_kind: Option<String>,
    pub hop: u32,
}

/// Breadth-first walk of the typed entity-relation graph.
///
/// Follows `entity_relations` edges **in both directions**, so a traversal from
/// "Bailey" surfaces relations Bailey is the subject of *and* relations naming
/// Bailey as the object.
///
/// This is a different thing from expanding a search via `memory_entities`:
/// that is 1-hop co-mention — two memories happen to name the same entity —
/// whereas this follows typed subject/relation/object triples, which is what
/// lets a question chain ("who introduced me to the person who recommended
/// this") actually resolve.
///
/// # Termination
///
/// Each hop queries only the entities *newly discovered* by the previous hop;
/// the seed set never re-enters a frontier. So an edge is never refetched once
/// both its endpoints have been visited, and a cycle simply produces an empty
/// next frontier. `seen_edges` exists for a narrower reason — one edge can be
/// returned twice within a single hop when both its endpoints sit in the same
/// frontier — not to bound the walk.
pub fn traverse_entities(
    conn: &Connection,
    seed_entity_ids: &[String],
    hops: u32,
    relation: Option<&str>,
    cap: usize,
) -> Result<Vec<RelationEdge>> {
    let mut seen_entities: std::collections::HashSet<String> =
        seed_entity_ids.iter().cloned().collect();
    let mut frontier: Vec<String> = seed_entity_ids.to_vec();
    let mut seen_edges = std::collections::HashSet::new();
    let mut edges = Vec::new();

    for hop in 1..=hops {
        if frontier.is_empty() || edges.len() >= cap {
            break;
        }

        let found = Entities::new(conn).relations_touching(&frontier, relation, hop)?;

        let mut next_frontier = Vec::new();
        for (edge_id, edge) in found {
            if !seen_edges.insert(edge_id) {
                continue;
            }
            if edges.len() >= cap {
                break;
            }
            for neighbour in [&edge.subject_entity_id, &edge.object_entity_id] {
                if seen_entities.insert(neighbour.clone()) {
                    next_frontier.push(neighbour.clone());
                }
            }
            edges.push(edge);
        }
        frontier = next_frontier;
    }

    Ok(edges)
}

/// Rewrite entity ids that predate [`entity_id`]'s current derivation.
///
/// Returns the number of rows rewritten. Idempotent: a database whose ids
/// already match is untouched, so this is safe to run on every open.
///
/// Ids used to be `ent_` plus the full 64-hex digest of a merely-trimmed name.
/// The reference uses the first 12 hex characters of the digest of a
/// whitespace-collapsed name, with no prefix, so every entity written by this
/// crate was invisible to `remind_me` and vice versa.
///
/// Nothing cascades — the reference declares no foreign key on `memory_entities`
/// or `entity_relations`, so that sync can deliver rows out of order — which
/// means the referencing columns have to be repointed explicitly or every link
/// dangles.
///
/// Two rows can collapse onto one id, because names differing only by internal
/// whitespace used to be distinct entities. Those are merged rather than left
/// to collide on the primary key: aliases union, the earliest `created_at`
/// wins, and a `kind` already set is kept.
pub fn renormalize_entity_ids(conn: &Connection) -> Result<usize> {
    let entities = Entities::new(conn);
    let existing = entities.all_oldest_first()?;

    let mut rewritten = 0;
    for entity in existing {
        let want = entity_id(&entity.name);
        if want == entity.id {
            continue;
        }

        match entities.get(&want)? {
            None => entities.rename(&entity.id, &want)?,
            Some(target) => {
                let merged = dedup_preserving_order(
                    target.aliases.iter().cloned().chain(entity.aliases.clone()),
                );
                let kind = target.kind.clone().or_else(|| entity.kind.clone());
                let created_at = target.created_at.min(entity.created_at.clone());
                entities.set_merged(&want, kind.as_deref(), &merged, &created_at)?;
                entities.delete(&entity.id)?;
            }
        }
        entities.repoint(&entity.id, &want)?;

        rewritten += 1;
    }

    Ok(rewritten)
}

/// A summary of one entity, as it appears in a traversal payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRef {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
}

/// The payload of `remind_me_entity_traverse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityTraverseResult {
    pub found: bool,
    /// Echoed back when nothing resolved, so a caller can see what was tried.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<EntityRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hops: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<RelationEdge>,
    /// Every entity touched, the seed first. De-duplicated in discovery order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<EntityRef>,
}

/// Resolve the start node, walk the relation graph, and collect the entities
/// touched.
///
/// `hops` and `cap` are **clamped** rather than rejected: the reference bounds
/// them in its input schema, and a caller that ignores the schema should get a
/// bounded walk rather than an error.
///
/// An unresolvable start node is `found: false` with a message, not an error —
/// "no such entity" is an ordinary answer to this question.
pub fn traverse_from_name(
    conn: &Connection,
    input: &crate::models::EntityTraverseInput,
) -> Result<EntityTraverseResult> {
    let seed = match resolve_entity(conn, &input.name)? {
        Some(entity) => entity,
        None => {
            return Ok(EntityTraverseResult {
                found: false,
                query: Some(input.name.clone()),
                message: Some(format!("No entity found matching {:?}.", input.name)),
                entity: None,
                hops: None,
                edges: Vec::new(),
                entities: Vec::new(),
            })
        }
    };

    let hops = input.hops.clamp(TRAVERSE_HOPS_MIN, TRAVERSE_HOPS_MAX);
    let cap = input.cap.clamp(1, 100);
    let edges = traverse_entities(
        conn,
        std::slice::from_ref(&seed.id),
        hops,
        input.relation.as_deref(),
        cap,
    )?;

    let mut entities = vec![EntityRef {
        id: seed.id.clone(),
        name: seed.name.clone(),
        kind: seed.kind.clone(),
    }];
    let mut seen: std::collections::HashSet<String> = [seed.id.clone()].into_iter().collect();
    for edge in &edges {
        for (id, name, kind) in [
            (
                &edge.subject_entity_id,
                &edge.subject_name,
                &edge.subject_kind,
            ),
            (&edge.object_entity_id, &edge.object_name, &edge.object_kind),
        ] {
            if seen.insert(id.clone()) {
                entities.push(EntityRef {
                    id: id.clone(),
                    name: name.clone(),
                    kind: kind.clone(),
                });
            }
        }
    }

    Ok(EntityTraverseResult {
        found: true,
        query: None,
        message: None,
        entity: Some(EntityRef {
            id: seed.id,
            name: seed.name,
            kind: seed.kind,
        }),
        hops: Some(hops),
        edges,
        entities,
    })
}

/// The deterministic id for a typed relation edge.
///
/// `sha256("subject_id|normalized_relation|object_id")` truncated to 12 hex
/// characters, matching the reference. The relation label is normalised for the
/// same reason entity names are — so two machines recording the same edge
/// converge on one row rather than two.
pub fn entity_relation_id(
    subject_entity_id: &str,
    relation: &str,
    object_entity_id: &str,
) -> String {
    let key = format!(
        "{}|{}|{}",
        subject_entity_id,
        normalize_entity_name(relation),
        object_entity_id
    );
    sha256::digest(key)[..12].to_string()
}

/// Record a typed edge between two entities. Returns `true` if it is new.
///
/// Insert-or-ignore on the derived id, so re-recording the same edge is a no-op
/// rather than an error. The stored label has its whitespace collapsed, matching
/// the id's normalisation.
pub fn upsert_entity_relation(
    conn: &Connection,
    subject_entity_id: &str,
    relation: &str,
    object_entity_id: &str,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let id = entity_relation_id(subject_entity_id, relation, object_entity_id);
    let label = relation.split_whitespace().collect::<Vec<_>>().join(" ");
    Entities::new(conn).insert_relation_or_ignore(&RelationRow {
        id: &id,
        subject_entity_id,
        relation: &label,
        object_entity_id,
        created_at: &now,
        updated_at: &now,
        node_id: Some(&crate::sync::configured_node_id()),
    })
}

/// Best-effort: record a relation edge when an SPO triple names two *known*
/// entities.
///
/// A memory's triple is free text — writing one does not imply the subject and
/// object name anything in the graph. An edge is only recorded when **both**
/// sides resolve to entities that already exist, typically because the same
/// call's `entities` list upserted them a moment earlier. A triple naming
/// something unknown keeps working exactly as before: a memory-level triple
/// with no graph edge, rather than an error or an invented entity.
///
/// Returns `true` when both sides resolved, whether or not the edge was new.
pub fn maybe_link_entity_relation(
    conn: &Connection,
    subject: Option<&str>,
    predicate: Option<&str>,
    object: Option<&str>,
) -> Result<bool> {
    let (Some(subject), Some(predicate), Some(object)) = (subject, predicate, object) else {
        return Ok(false);
    };
    if subject.trim().is_empty() || predicate.trim().is_empty() || object.trim().is_empty() {
        return Ok(false);
    }

    let (Some(subject_entity), Some(object_entity)) = (
        resolve_entity(conn, subject)?,
        resolve_entity(conn, object)?,
    ) else {
        return Ok(false);
    };

    upsert_entity_relation(conn, &subject_entity.id, predicate, &object_entity.id)?;
    Ok(true)
}

/// Supersede live facts that a new triple contradicts.
///
/// A memory sharing this triple's `(subject, predicate)` but carrying a
/// *different* `object` is a contradiction: "I moved to Boston" replaces "I
/// live in Seattle" even though the two share no words, which similarity-based
/// merging could never catch.
///
/// Returns the ids superseded.
///
/// # What this deliberately does not do
///
/// It is **not** predicate inference. `lives_in` does not contradict `visited`
/// — only an exact normalised `(subject, predicate)` match counts. A
/// differently-worded predicate for a related-but-distinct claim is a
/// false-positive risk the caller controls by choosing predicate names, not
/// something this tries to resolve.
///
/// A memory with the *same* object is the same fact restated, not a
/// contradiction, so it survives.
///
/// # Why the comparison is not in SQL
///
/// `lower()` in SQL would miss internal-whitespace variants, which
/// [`normalize_entity_name`] collapses. SQL narrows to live, fully-tripled
/// candidates; the exact comparison happens here, against the same
/// normalisation the entity graph uses for identity.
pub fn supersede_contradicting_facts(
    conn: &Connection,
    memory_id: &str,
    subject: Option<&str>,
    predicate: Option<&str>,
    object: Option<&str>,
) -> Result<Vec<String>> {
    let (Some(subject), Some(predicate), Some(object)) = (subject, predicate, object) else {
        return Ok(Vec::new());
    };
    if subject.is_empty() || predicate.is_empty() || object.is_empty() {
        return Ok(Vec::new());
    }

    let want_subject = normalize_entity_name(subject);
    let want_predicate = normalize_entity_name(predicate);
    let want_object = normalize_entity_name(object);

    let memories = Memories::new(conn);
    let candidates = memories.live_triples_except(memory_id)?;

    let now = Utc::now().to_rfc3339();
    let mut superseded = Vec::new();
    for triple in candidates {
        if normalize_entity_name(&triple.subject) != want_subject
            || normalize_entity_name(&triple.predicate) != want_predicate
        {
            continue;
        }
        if normalize_entity_name(&triple.object) == want_object {
            continue; // the same fact restated, not a contradiction
        }
        memories.set_superseded_by(&triple.id, memory_id, Some(&now))?;
        superseded.push(triple.id);
    }
    Ok(superseded)
}
