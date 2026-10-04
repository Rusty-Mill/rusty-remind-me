//! The store operations the CLI asks of the daemon.
//!
//! The daemon speaks the node's own operations, not SQL (ADR-0023 §2): MCP
//! clients send MCP tool calls, and the CLI sends these. Each one is a store
//! call the CLI used to make directly; its result travels back as the same
//! typed value, so the CLI renders it exactly as before.
//!
//! [`execute`] is also what the CLI runs in-process, so both paths share one
//! implementation and one serialization round trip.

use crate::db::Store;
use crate::models::{EntityInput, MemoryAddInput, MemoryListInput, MemorySearchInput};
use crate::{db::queries, entity, session_ops, stats, wiki, wiki_import};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Search {
        input: MemorySearchInput,
    },
    Add {
        input: MemoryAddInput,
    },
    List {
        input: MemoryListInput,
    },
    Get {
        id: String,
    },
    UpsertEntity {
        input: EntityInput,
    },
    WikiWrite {
        slug: String,
        title: String,
        content: String,
    },
    WikiRead {
        slug: String,
    },
    /// `dir` must be absolute: the daemon's working directory is not the
    /// client's.
    WikiImport {
        dir: PathBuf,
    },
    Stats,
    /// The session subcommands (`crate::session_ops`). Paths are absolute.
    SessionStart {
        session_id: String,
        cwd: PathBuf,
        client: Option<String>,
    },
    SessionEnd {
        session_id: String,
        cwd: PathBuf,
        reason: Option<String>,
    },
    CaptureTranscript {
        path: PathBuf,
        session_id: String,
        cwd: Option<PathBuf>,
        reason: Option<String>,
    },
    Context {
        cwd: Option<PathBuf>,
        project: Option<String>,
        branch: Option<String>,
        prompt: Option<String>,
        budget: usize,
    },
    SessionTimeline {
        session_id: String,
    },
    /// The daemon's own state. Answered by the daemon, not [`execute`].
    Status,
    /// Stop the daemon. Answered by the daemon, not [`execute`].
    Shutdown,
}

/// The answer to one [`Op`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpReply {
    Ok(Value),
    Err(String),
}

impl OpReply {
    /// The result as `T`, or the operation's error.
    pub fn into_result<T: DeserializeOwned>(self) -> Result<T, String> {
        match self {
            OpReply::Ok(value) => serde_json::from_value(value).map_err(|e| e.to_string()),
            OpReply::Err(message) => Err(message),
        }
    }
}

/// Run a store operation against `store`.
pub fn execute(store: &Store<'_>, op: &Op) -> OpReply {
    match run(store, op) {
        Ok(value) => OpReply::Ok(value),
        Err(message) => OpReply::Err(message),
    }
}

fn run(store: &Store<'_>, op: &Op) -> Result<Value, String> {
    match op {
        Op::Search { input } => to_value(queries::search_memories(store, input)),
        Op::Add { input } => to_value(queries::add_memory(store, input.clone())),
        Op::List { input } => to_value(queries::list_memories(store, input)),
        Op::Get { id } => to_value(queries::get_memory_by_id(store, id)),
        Op::UpsertEntity { input } => to_value(entity::upsert_entity(store, input)),
        Op::WikiWrite {
            slug,
            title,
            content,
        } => to_value(wiki::write_wiki_page(store, slug, title, content, "")),
        Op::WikiRead { slug } => to_value(wiki::get_wiki_page(store, slug)),
        Op::WikiImport { dir } => {
            let report =
                wiki_import::import_wiki_dir(store, dir, true).map_err(|e| e.to_string())?;
            serde_json::to_value(report).map_err(|e| e.to_string())
        }
        Op::Stats => to_value(stats::collect(store)),
        Op::SessionStart {
            session_id,
            cwd,
            client,
        } => session_ops::session_start(store, session_id, cwd, client.as_deref())
            .map_err(|e| e.to_string()),
        Op::SessionEnd {
            session_id,
            cwd,
            reason,
        } => session_ops::session_end(store, session_id, cwd, reason.as_deref())
            .map_err(|e| e.to_string()),
        Op::CaptureTranscript {
            path,
            session_id,
            cwd,
            reason,
        } => session_ops::capture_transcript(
            store,
            path,
            session_id,
            cwd.as_deref(),
            reason.as_deref(),
        )
        .map_err(|e| e.to_string()),
        Op::Context {
            cwd,
            project,
            branch,
            prompt,
            budget,
        } => session_ops::context(
            store,
            &session_ops::ContextArgs {
                cwd: cwd.as_deref(),
                project: project.as_deref(),
                branch: branch.as_deref(),
                prompt: prompt.as_deref(),
                budget: *budget,
            },
        )
        .map_err(|e| e.to_string()),
        Op::SessionTimeline { session_id } => {
            session_ops::session_timeline(store, session_id).map_err(|e| e.to_string())
        }
        Op::Status | Op::Shutdown => Err("only a daemon answers status and shutdown".into()),
    }
}

fn to_value<T: Serialize>(result: crate::db::Result<T>) -> Result<Value, String> {
    let value = result.map_err(|e| e.to_string())?;
    serde_json::to_value(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Memory;
    use crate::Database;

    fn add(store: &Store<'_>, content: &str) -> Memory {
        let input = MemoryAddInput {
            sensitive: false,
            content: content.into(),
            category: "note".into(),
            tags: vec![],
            source: "cli".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        };
        execute(store, &Op::Add { input }).into_result().unwrap()
    }

    #[test]
    fn ops_survive_the_wire_both_ways() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let added = add(&store, "the wire keeps its shape");

        let op = Op::Get {
            id: added.id.clone(),
        };
        let op: Op = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        let reply = execute(&store, &op);
        let reply: OpReply = serde_json::from_str(&serde_json::to_string(&reply).unwrap()).unwrap();
        let got: Option<Memory> = reply.into_result().unwrap();
        let got = got.expect("the memory");
        // Serialized identically, so the CLI prints the same bytes either way.
        assert_eq!(
            serde_json::to_string(&got).unwrap(),
            serde_json::to_string(&added).unwrap()
        );
    }

    #[test]
    fn a_missing_row_is_ok_none_and_control_ops_are_refused() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let got: Option<Memory> = execute(&store, &Op::Get { id: "nope".into() })
            .into_result()
            .unwrap();
        assert!(got.is_none());
        assert!(matches!(execute(&store, &Op::Shutdown), OpReply::Err(_)));
    }
}
