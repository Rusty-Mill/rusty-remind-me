//! Memory export: a complete logical backup, portable and re-importable.
//!
//! Distinct from [`crate::backup`], which copies the SQLite file. An export is
//! filterable, human-readable, and consumable on another machine.
//!
//! # What is and is not included
//!
//! **Every column of `memories`**, including lifecycle fields — `vitality`,
//! `superseded_by`, `access_count`. The point is a backup, not a view, so
//! superseded and deleted rows are exported too rather than filtered the way
//! search filters them.
//!
//! **Embedding vectors are deliberately excluded.** They are derived data,
//! rebuildable on the target machine, so carrying them would bloat the file for
//! nothing.
//!
//! # Round-tripping is lossy in one specific way
//!
//! Each record carries a `role`/`content` pair so the file is directly
//! consumable by the chat importer. But a re-import re-chunks long content and
//! assigns fresh ids, category, tags and source — the original values are still
//! in the file for manual restoration, but a naive round-trip does not preserve
//! them.

use crate::db::queries::parse_memory_row;
use crate::models::{ExportFormat, ExportInput, ExportResult};
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, Result};
use std::path::{Path, PathBuf};

/// Environment variable listing the roots an export may write to, in the
/// platform's own `PATH`-list syntax (see
/// [`crate::import_paths::split_path_list`]).
pub const EXPORT_ROOTS_ENV: &str = "REMIND_ME_EXPORT_ROOTS";

/// Roots an export destination must be contained in.
///
/// Defaults to the user's home directory, matching the reference.
pub fn export_roots() -> Vec<PathBuf> {
    match std::env::var(EXPORT_ROOTS_ENV) {
        Ok(raw) if !raw.trim().is_empty() => crate::import_paths::split_path_list(&raw)
            .into_iter()
            .map(|r| crate::import_paths::resolve_lexically(&PathBuf::from(expand_home(&r))))
            .collect(),
        // See `import_paths::roots_from`'s matching comment: the root must
        // be resolved the same way a candidate path is, or `\\?\`-prefixed
        // canonicalized candidates never match an un-resolved root on
        // Windows.
        _ => crate::import_paths::home_dir_var()
            .map(|home| vec![crate::import_paths::resolve_lexically(&PathBuf::from(home))])
            .unwrap_or_default(),
    }
}

fn expand_home(raw: &str) -> String {
    match (raw.strip_prefix("~/"), crate::import_paths::home_dir_var()) {
        (Some(rest), Ok(home)) => format!("{}/{}", home.trim_end_matches('/'), rest),
        _ => raw.to_string(),
    }
}

/// Why an export destination was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportPathError {
    OutsideRoots(PathBuf),
    IsADirectory(PathBuf),
    NoParentDirectory(PathBuf),
}

impl std::fmt::Display for ExportPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRoots(p) => {
                write!(f, "Path not in allowed export roots: {}", p.display())
            }
            Self::IsADirectory(p) => {
                write!(f, "Destination is a directory, not a file: {}", p.display())
            }
            Self::NoParentDirectory(p) => {
                write!(f, "Parent directory not found: {}", p.display())
            }
        }
    }
}

impl std::error::Error for ExportPathError {}

/// Anything that can go wrong during an export.
#[derive(Debug)]
pub enum ExportError {
    Db(rusqlite::Error),
    Path(ExportPathError),
    Io(std::io::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "{}", e),
            Self::Path(e) => write!(f, "{}", e),
            Self::Io(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<rusqlite::Error> for ExportError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

impl From<ExportPathError> for ExportError {
    fn from(e: ExportPathError) -> Self {
        Self::Path(e)
    }
}

impl From<std::io::Error> for ExportError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Resolve and validate an export destination.
///
/// **Containment is checked first, before anything that touches the
/// filesystem.** That ordering is the security property, not an accident: a
/// check that tested existence first would answer "does this file exist?" for
/// any path on the machine, turning the export tool into a filesystem oracle.
/// The reference is explicit about this for its import roots (`SE-02`) and
/// mirrors it here.
///
/// The path is resolved before the containment test, so `..` segments and
/// symlinks cannot step outside a root.
pub fn validate_export_path(raw: &str) -> std::result::Result<PathBuf, ExportPathError> {
    let expanded = PathBuf::from(expand_home(raw.trim()));
    // The destination itself will not exist yet, so resolve the deepest
    // existing ancestor and rebuild — `canonicalize` on a missing file fails.
    let resolved = crate::import_paths::resolve_lexically(&expanded);

    let roots = export_roots();
    if !crate::import_paths::is_contained(&resolved, &roots) {
        return Err(ExportPathError::OutsideRoots(resolved));
    }
    if resolved.is_dir() {
        return Err(ExportPathError::IsADirectory(resolved));
    }
    match resolved.parent() {
        Some(parent) if parent.is_dir() => Ok(resolved),
        Some(parent) => Err(ExportPathError::NoParentDirectory(parent.to_path_buf())),
        None => Err(ExportPathError::NoParentDirectory(resolved)),
    }
}

/// Strip Windows' `\\?\` verbatim-path prefix for display/reporting
/// purposes only. `resolve_lexically`'s `canonicalize()` call adds that
/// prefix on Windows (needed so long paths and symlink resolution both
/// work correctly) but it is an implementation detail with no business
/// leaking into a caller-facing result string like [`ExportResult::file`].
/// A plain prefix strip (rather than rewriting `\\?\UNC\` to `\\`) is
/// enough here: every path this module resolves descends from
/// [`crate::import_paths::home_dir_var`] or a caller-supplied
/// relative/absolute path, never a UNC share.
#[cfg(windows)]
fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

#[cfg(not(windows))]
fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn filters(input: &ExportInput) -> (String, Vec<Value>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut bindings: Vec<Value> = Vec::new();

    // Both conditions, not just `deleted_at` -- the reference gates the pair
    // on this one flag (`exporter.py:163`), and a superseded memory is just as
    // resurrectable as a tombstoned one: every exported record carries
    // `role: "assistant"`, so the importer reads it back as live content.
    if !input.include_deleted {
        conditions.push("m.deleted_at IS NULL".to_string());
        conditions.push("m.superseded_by IS NULL".to_string());
    }

    if let Some(category) = input.category.as_ref().filter(|c| !c.is_empty()) {
        conditions.push("m.category = ?".to_string());
        bindings.push(Value::Text(category.clone()));
    }
    // ALL-of tag semantics, against the normalized junction table — the same
    // shape `list_memories` uses.
    for tag in input.tags.iter().flatten() {
        conditions.push(
            "EXISTS (SELECT 1 FROM memory_tags mt WHERE mt.memory_id = m.id AND mt.tag = ?)"
                .to_string(),
        );
        bindings.push(Value::Text(tag.clone()));
    }

    let clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    (clause, bindings)
}

/// Collect the entity graph as `record_type`-tagged records.
///
/// Entities are emitted first, so a sequential restore can verify that a
/// link's or relation's endpoints exist before it applies them.
///
/// When `memory_ids` is `Some` — a filtered export — the graph is scoped to
/// what is reachable from the exported memories: links whose memory is in the
/// set, entities those links reference, and relations whose subject **and**
/// object are both among those entities. Exporting an edge with one endpoint
/// outside the set would produce a dangling reference on restore.
fn collect_graph_records(
    conn: &Connection,
    memory_ids: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<serde_json::Value>> {
    let mut links: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT memory_id, entity_id, created_at FROM memory_entities
              ORDER BY created_at, memory_id, entity_id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>>>()?;
        rows
    };
    let mut entities: Vec<serde_json::Value> = {
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, aliases, created_at, updated_at FROM entities
              ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let aliases_json: String = r.get(3)?;
                Ok(serde_json::json!({
                    "record_type": "entity",
                    "id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "kind": r.get::<_, Option<String>>(2)?,
                    "aliases": serde_json::from_str::<Vec<String>>(&aliases_json)
                        .unwrap_or_default(),
                    "created_at": r.get::<_, String>(4)?,
                    "updated_at": r.get::<_, String>(5)?,
                }))
            })?
            .collect::<Result<Vec<_>>>()?;
        rows
    };
    let mut relations: Vec<serde_json::Value> = {
        let mut stmt = conn.prepare(
            "SELECT id, subject_entity_id, relation, object_entity_id, created_at, updated_at
               FROM entity_relations ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "record_type": "entity_relation",
                    "id": r.get::<_, String>(0)?,
                    "subject_entity_id": r.get::<_, String>(1)?,
                    "relation": r.get::<_, String>(2)?,
                    "object_entity_id": r.get::<_, String>(3)?,
                    "created_at": r.get::<_, String>(4)?,
                    "updated_at": r.get::<_, String>(5)?,
                }))
            })?
            .collect::<Result<Vec<_>>>()?;
        rows
    };

    if let Some(ids) = memory_ids {
        links.retain(|(memory_id, _, _)| ids.contains(memory_id));
        let linked: std::collections::HashSet<String> = links
            .iter()
            .map(|(_, entity_id, _)| entity_id.clone())
            .collect();
        entities.retain(|e| {
            e.get("id")
                .and_then(|v| v.as_str())
                .map(|id| linked.contains(id))
                .unwrap_or(false)
        });
        relations.retain(|r| {
            let subject = r.get("subject_entity_id").and_then(|v| v.as_str());
            let object = r.get("object_entity_id").and_then(|v| v.as_str());
            matches!((subject, object), (Some(s), Some(o)) if linked.contains(s) && linked.contains(o))
        });
    }

    let mut records = entities;
    records.extend(links.into_iter().map(|(memory_id, entity_id, created_at)| {
        serde_json::json!({
            "record_type": "memory_entity",
            "memory_id": memory_id,
            "entity_id": entity_id,
            "created_at": created_at,
        })
    }));
    records.extend(relations);
    Ok(records)
}

/// Collect memory records, and the graph when asked for.
pub fn collect_export_records(
    conn: &Connection,
    input: &ExportInput,
) -> Result<Vec<serde_json::Value>> {
    let (where_clause, bindings) = filters(input);
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories m {} ORDER BY m.created_at, m.id",
        crate::db::queries::prefixed_memory_columns("m"),
        where_clause
    ))?;
    let memories: Vec<crate::models::Memory> = stmt
        .query_map(params_from_iter(bindings.iter()), parse_memory_row)?
        .collect::<Result<_>>()?;
    drop(stmt);

    let mut records: Vec<serde_json::Value> = memories
        .iter()
        .map(|memory| {
            let mut record = serde_json::to_value(memory).unwrap_or(serde_json::Value::Null);
            if let Some(object) = record.as_object_mut() {
                // Purely for importer compatibility: the importer's default
                // extract mode keeps assistant-role content verbatim, so a
                // re-import preserves memory content losslessly.
                object.insert("role".into(), serde_json::json!("assistant"));
            }
            record
        })
        .collect();

    if input.include_graph {
        let filtered = input.category.is_some() || input.tags.is_some();
        let ids: Option<std::collections::HashSet<String>> =
            filtered.then(|| memories.iter().map(|m| m.id.clone()).collect());
        records.extend(collect_graph_records(conn, ids.as_ref())?);
    }
    Ok(records)
}

/// Serialise records in the requested format.
pub fn render_export(records: &[serde_json::Value], format: ExportFormat) -> String {
    match format {
        ExportFormat::Json => {
            serde_json::to_string_pretty(records).unwrap_or_else(|_| "[]".to_string())
        }
        ExportFormat::Jsonl => records
            .iter()
            .map(|r| format!("{}\n", r))
            .collect::<String>(),
    }
}

/// Export memories, and optionally the entity graph, inline or to a file.
///
/// `file_path` is validated against [`export_roots`] before anything is
/// written. When omitted the payload is returned inline.
pub fn export_memories(
    conn: &Connection,
    input: &ExportInput,
) -> std::result::Result<ExportResult, ExportError> {
    let records = collect_export_records(conn, input)?;
    let payload = render_export(&records, input.format);

    let count_of = |kind: &str| -> usize {
        records
            .iter()
            .filter(|r| r.get("record_type").and_then(|v| v.as_str()) == Some(kind))
            .count()
    };
    let entities = count_of("entity");
    let links = count_of("memory_entity");
    let relations = count_of("entity_relation");
    let exported = records.len() - entities - links - relations;

    let mut result = ExportResult {
        status: "ok",
        exported,
        format: input.format,
        entities: input.include_graph.then_some(entities),
        links: input.include_graph.then_some(links),
        relations: input.include_graph.then_some(relations),
        file: None,
        bytes: None,
        content: None,
    };

    match input.file_path.as_ref().filter(|p| !p.trim().is_empty()) {
        Some(raw) => {
            let path = validate_export_path(raw)?;
            // Bytes rather than text, so no platform newline translation makes
            // the file differ from the byte count reported here — an export is
            // meant to be identical across platforms for diffing and hashing.
            std::fs::write(&path, payload.as_bytes())?;
            result.bytes = Some(payload.len());
            result.file = Some(display_path(&path));
        }
        None => result.content = Some(payload),
    }

    Ok(result)
}
