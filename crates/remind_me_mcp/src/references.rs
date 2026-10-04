//! `memory_references` on the tool surface: the `references` field on
//! `remind_me_get` / `remind_me_search` results, and the reverse-lookup tool
//! `remind_me_references` ("everything about rusty_mill#321").
//!
//! Kept out of `lib.rs` so the dispatcher only calls in; the lookups go
//! through [`References::for_memories`], one pass for a whole result page.

use remind_me_core::db::references::{MemoryReference, References};
use remind_me_core::db::{queries, Store};
use remind_me_core::expansion::MemorySearchResponse;
use remind_me_core::Memory;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// How many references the Markdown view lists per memory.
const MARKDOWN_REFS: usize = 5;
/// Rows the reverse lookup returns at most.
const LOOKUP_LIMIT: usize = 100;
/// Reference kinds, for a lookup that names a value but no kind.
const KINDS: [&str; 7] = [
    "url",
    "issue",
    "pull",
    "commit",
    "path",
    "handle",
    "attachment",
];

fn by_memory(rows: Vec<MemoryReference>) -> BTreeMap<String, Vec<MemoryReference>> {
    let mut grouped: BTreeMap<String, Vec<MemoryReference>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.memory_id.clone()).or_default().push(row);
    }
    grouped
}

fn grouped_for(
    store: &Store<'_>,
    memories: &[&Memory],
) -> Result<BTreeMap<String, Vec<MemoryReference>>, String> {
    let ids: Vec<String> = memories.iter().map(|m| m.id.clone()).collect();
    References::new(store)
        .for_memories(&ids)
        .map(by_memory)
        .map_err(|e| e.to_string())
}

fn refs_json(rows: Option<&Vec<MemoryReference>>) -> Value {
    serde_json::to_value(rows.map(Vec::as_slice).unwrap_or(&[])).unwrap_or_else(|_| json!([]))
}

/// A memory as JSON with its `references`.
pub fn memory_json(store: &Store<'_>, memory: &Memory) -> Result<Value, String> {
    let grouped = grouped_for(store, &[memory])?;
    let mut value = serde_json::to_value(memory).map_err(|e| e.to_string())?;
    value["references"] = refs_json(grouped.get(&memory.id));
    Ok(value)
}

/// A search response as JSON with `references` on each result's memory.
pub fn search_json(store: &Store<'_>, res: &MemorySearchResponse) -> Result<Value, String> {
    let memories: Vec<&Memory> = res.memories.iter().map(|r| &r.memory).collect();
    let grouped = grouped_for(store, &memories)?;
    let mut value = serde_json::to_value(res).map_err(|e| e.to_string())?;
    if let Some(results) = value["memories"].as_array_mut() {
        for (result, memory) in results.iter_mut().zip(&memories) {
            result["memory"]["references"] = refs_json(grouped.get(&memory.id));
        }
    }
    Ok(value)
}

fn ref_label(r: &MemoryReference) -> String {
    format!("{}:{}", r.kind, r.value)
}

/// A `refs:` line per memory that has any (up to five each), to follow the
/// Markdown result list. Empty when no result has a reference.
pub fn search_markdown_footer(store: &Store<'_>, res: &MemorySearchResponse) -> String {
    let memories: Vec<&Memory> = res.memories.iter().map(|r| &r.memory).collect();
    let Ok(grouped) = grouped_for(store, &memories) else {
        return String::new();
    };
    let lines: Vec<String> = memories
        .iter()
        .filter_map(|m| {
            let rows = grouped.get(&m.id)?;
            let listed: Vec<String> = rows.iter().take(MARKDOWN_REFS).map(ref_label).collect();
            Some(format!("- `{}` refs: {}", m.id, listed.join(", ")))
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("\n\n**References**\n{}", lines.join("\n"))
}

/// Input schema entry for `tools/list`.
pub fn tool_schema() -> Value {
    json!({
        "name": "remind_me_references",
        "description": "Reverse lookup of what memories point at. Give `memory_id` for everything one memory references, or `kind` and/or `value` for every memory that references a thing (e.g. value \"rusty_mill/x#321\", or kind \"commit\"). Kinds: url, issue, pull, commit, path, handle, attachment.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": KINDS },
                "value": { "type": "string", "description": "The normalised value: owner/repo#123, a commit sha, a path, a URL, @handle, sha256:<hex>." },
                "memory_id": { "type": "string" }
            }
        }
    })
}

fn lookup(store: &Store<'_>, args: &Value) -> Result<Vec<MemoryReference>, String> {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let refs = References::new(store);
    let found = match (text("memory_id"), text("kind"), text("value")) {
        (Some(id), _, _) => refs.for_memory(id),
        (None, Some(kind), Some(value)) => refs.find(kind, value),
        (None, Some(kind), None) => refs.of_kind(kind),
        (None, None, Some(value)) => KINDS
            .iter()
            .map(|k| refs.find(k, value))
            .collect::<Result<Vec<_>, _>>()
            .map(|all| all.into_iter().flatten().collect()),
        (None, None, None) => return Err("give memory_id, kind or value".to_string()),
    };
    found.map_err(|e| e.to_string())
}

/// Run the tool: each reference plus a short view of the memory it points
/// from (a memory that was since deleted is shown as `null`).
pub fn run(store: &Store<'_>, args: &Value) -> Result<Value, String> {
    let mut rows = lookup(store, args)?;
    let total = rows.len();
    rows.truncate(LOOKUP_LIMIT);
    let mut memories: BTreeMap<String, Value> = BTreeMap::new();
    for row in &rows {
        if memories.contains_key(&row.memory_id) {
            continue;
        }
        let view = match queries::get_memory_by_id(store, &row.memory_id) {
            Ok(Some(m)) => json!({
                "id": m.id,
                "category": m.category,
                "content": m.content.chars().take(200).collect::<String>(),
                "created_at": m.created_at,
            }),
            _ => Value::Null,
        };
        memories.insert(row.memory_id.clone(), view);
    }
    let references: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
            v["memory"] = memories.get(&r.memory_id).cloned().unwrap_or(Value::Null);
            v
        })
        .collect();
    Ok(json!({ "count": total, "returned": references.len(), "references": references }))
}
