use crate::models::EntityInput;
use chrono::Utc;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Result};
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
    match get_entity_by_id(conn, &id)? {
        None => {
            conn.execute(
                "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at, node_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    id,
                    name,
                    input.kind,
                    serde_json::to_string(&clean_aliases).unwrap_or_else(|_| "[]".to_string()),
                    now,
                    now,
                    crate::sync::configured_node_id(),
                ],
            )?;
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
                conn.execute(
                    "UPDATE entities SET kind = ?, aliases = ?, updated_at = ? WHERE id = ?",
                    params![
                        new_kind,
                        serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string()),
                        now,
                        id
                    ],
                )?;
            }
        }
    }

    get_entity_by_id(conn, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// Fetch an entity by its deterministic id.
pub fn get_entity_by_id(conn: &Connection, id: &str) -> Result<Option<Entity>> {
    let mut stmt = conn.prepare(&format!("{} WHERE id = ?", ENTITY_SELECT))?;
    let mut rows = stmt.query_map(params![id], parse_entity_row)?;
    if let Some(row) = rows.next() {
        row.map(Some)
    } else {
        Ok(None)
    }
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
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id, created_at)
         VALUES (?, ?, ?)",
        params![memory_id, entity_id, Utc::now().to_rfc3339()],
    )?;
    Ok(inserted > 0)
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

const ENTITY_SELECT: &str = "SELECT id, name, kind, aliases, created_at, updated_at FROM entities";

fn parse_entity_row(row: &rusqlite::Row) -> Result<Entity> {
    let aliases_json: String = row.get("aliases")?;
    let aliases: Vec<String> = serde_json::from_str(&aliases_json).unwrap_or_default();
    Ok(Entity {
        id: row.get("id")?,
        name: row.get("name")?,
        kind: row.get("kind")?,
        aliases,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
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

    let mut stmt = conn.prepare(ENTITY_SELECT)?;
    let rows = stmt.query_map([], parse_entity_row)?;
    let mut alias_hit: Option<Entity> = None;
    for row in rows {
        let entity = row?;
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
    let mut stmt = conn.prepare(
        "SELECT id, content, subject, predicate, object, category, created_at
           FROM memories
          WHERE superseded_by IS NULL AND deleted_at IS NULL
            AND (lower(subject) = ? OR lower(object) = ?)
          ORDER BY created_at DESC
          LIMIT ?",
    )?;
    let facts = stmt
        .query_map(params![canonical, canonical, limit as i64], |row| {
            Ok(EntityFact {
                id: row.get("id")?,
                content: row.get("content")?,
                subject: row.get("subject")?,
                predicate: row.get("predicate")?,
                object: row.get("object")?,
                category: row.get("category")?,
                created_at: row.get("created_at")?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    drop(stmt);

    let mut stmt = conn.prepare(
        "SELECT m.id, substr(m.content, 1, 300) AS content_snippet, m.category, m.created_at
           FROM memory_entities me
           JOIN memories m ON m.id = me.memory_id
          WHERE me.entity_id = ? AND m.superseded_by IS NULL AND m.deleted_at IS NULL
          ORDER BY m.created_at DESC
          LIMIT ?",
    )?;
    let memories = stmt
        .query_map(params![entity.id, limit as i64], |row| {
            Ok(EntityLinkedMemory {
                id: row.get("id")?,
                content_snippet: row.get("content_snippet")?,
                category: row.get("category")?,
                created_at: row.get("created_at")?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    drop(stmt);

    let total_linked_memories: usize = conn.query_row(
        "SELECT count(*)
           FROM memory_entities me
           JOIN memories m ON m.id = me.memory_id
          WHERE me.entity_id = ? AND m.superseded_by IS NULL AND m.deleted_at IS NULL",
        params![entity.id],
        |row| row.get::<_, i64>(0),
    )? as usize;

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
    let total: i64 = conn.query_row("SELECT count(*) FROM entities", [], |row| row.get(0))?;

    let mut stmt = conn.prepare(
        "SELECT e.id, e.name, e.kind, e.aliases, e.updated_at,
                count(me.memory_id) AS mention_count
           FROM entities e
      LEFT JOIN memory_entities me ON me.entity_id = e.id
       GROUP BY e.id
       ORDER BY mention_count DESC, e.name ASC
          LIMIT ? OFFSET ?",
    )?;
    let entities = stmt
        .query_map(params![limit as i64, offset as i64], |row| {
            let aliases_json: String = row.get("aliases")?;
            Ok(EntityListItem {
                id: row.get("id")?,
                name: row.get("name")?,
                kind: row.get("kind")?,
                aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
                updated_at: row.get("updated_at")?,
                mention_count: row.get("mention_count")?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;

    let count = entities.len();
    Ok(EntityListResult {
        total: total.max(0) as usize,
        count,
        offset,
        limit,
        has_more: total.max(0) as usize > offset + count,
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

        let placeholders = vec!["?"; frontier.len()].join(",");
        let relation_clause = if relation.is_some() {
            " AND r.relation = ?"
        } else {
            ""
        };
        let sql = format!(
            "SELECT r.id, r.subject_entity_id, r.relation, r.object_entity_id,
                    s.name AS subject_name, s.kind AS subject_kind,
                    o.name AS object_name, o.kind AS object_kind
               FROM entity_relations r
               JOIN entities s ON s.id = r.subject_entity_id
               JOIN entities o ON o.id = r.object_entity_id
              WHERE (r.subject_entity_id IN ({p}) OR r.object_entity_id IN ({p})){rel}
              ORDER BY r.created_at",
            p = placeholders,
            rel = relation_clause
        );

        // The frontier is bound twice — once per side of the OR.
        let mut bindings: Vec<Value> = frontier
            .iter()
            .chain(frontier.iter())
            .map(|id| Value::Text(id.clone()))
            .collect();
        if let Some(label) = relation {
            bindings.push(Value::Text(label.to_string()));
        }

        let mut stmt = conn.prepare(&sql)?;
        let found: Vec<(String, RelationEdge)> = stmt
            .query_map(params_from_iter(bindings), |row| {
                Ok((
                    row.get::<_, String>("id")?,
                    RelationEdge {
                        subject_entity_id: row.get("subject_entity_id")?,
                        subject_name: row.get("subject_name")?,
                        subject_kind: row.get("subject_kind")?,
                        relation: row.get("relation")?,
                        object_entity_id: row.get("object_entity_id")?,
                        object_name: row.get("object_name")?,
                        object_kind: row.get("object_kind")?,
                        hop,
                    },
                ))
            })?
            .collect::<Result<_>>()?;
        drop(stmt);

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
    let mut stmt = conn.prepare(&format!("{} ORDER BY created_at, id", ENTITY_SELECT))?;
    let existing: Vec<Entity> = stmt
        .query_map([], parse_entity_row)?
        .collect::<Result<_>>()?;
    drop(stmt);

    let mut rewritten = 0;
    for entity in existing {
        let want = entity_id(&entity.name);
        if want == entity.id {
            continue;
        }

        match get_entity_by_id(conn, &want)? {
            None => {
                conn.execute(
                    "UPDATE entities SET id = ? WHERE id = ?",
                    params![want, entity.id],
                )?;
            }
            Some(target) => {
                let merged = dedup_preserving_order(
                    target.aliases.iter().cloned().chain(entity.aliases.clone()),
                );
                let kind = target.kind.clone().or_else(|| entity.kind.clone());
                let created_at = target.created_at.min(entity.created_at.clone());
                conn.execute(
                    "UPDATE entities SET kind = ?, aliases = ?, created_at = ? WHERE id = ?",
                    params![
                        kind,
                        serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string()),
                        created_at,
                        want
                    ],
                )?;
                conn.execute("DELETE FROM entities WHERE id = ?", params![entity.id])?;
            }
        }

        // `memory_entities` is keyed `(memory_id, entity_id)`, so repointing can
        // collide with a link the surviving entity already has. Ignore those,
        // then drop whatever the ignore left behind.
        conn.execute(
            "UPDATE OR IGNORE memory_entities SET entity_id = ? WHERE entity_id = ?",
            params![want, entity.id],
        )?;
        conn.execute(
            "DELETE FROM memory_entities WHERE entity_id = ?",
            params![entity.id],
        )?;
        // Relations are keyed on their own id, so these cannot collide.
        conn.execute(
            "UPDATE entity_relations SET subject_entity_id = ? WHERE subject_entity_id = ?",
            params![want, entity.id],
        )?;
        conn.execute(
            "UPDATE entity_relations SET object_entity_id = ? WHERE object_entity_id = ?",
            params![want, entity.id],
        )?;

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
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO entity_relations
             (id, subject_entity_id, relation, object_entity_id, created_at, updated_at, node_id)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            id,
            subject_entity_id,
            label,
            object_entity_id,
            now,
            now,
            crate::sync::configured_node_id(),
        ],
    )?;
    Ok(inserted > 0)
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

    let mut stmt = conn.prepare(
        "SELECT id, subject, predicate, object FROM memories
          WHERE id != ?
            AND superseded_by IS NULL AND deleted_at IS NULL
            AND subject IS NOT NULL AND predicate IS NOT NULL AND object IS NOT NULL",
    )?;
    let candidates: Vec<(String, String, String, String)> = stmt
        .query_map(params![memory_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_>>()?;
    drop(stmt);

    let now = Utc::now().to_rfc3339();
    let mut superseded = Vec::new();
    for (id, candidate_subject, candidate_predicate, candidate_object) in candidates {
        if normalize_entity_name(&candidate_subject) != want_subject
            || normalize_entity_name(&candidate_predicate) != want_predicate
        {
            continue;
        }
        if normalize_entity_name(&candidate_object) == want_object {
            continue; // the same fact restated, not a contradiction
        }
        conn.execute(
            "UPDATE memories SET superseded_by = ?, updated_at = ? WHERE id = ?",
            params![memory_id, now, id],
        )?;
        superseded.push(id);
    }
    Ok(superseded)
}
