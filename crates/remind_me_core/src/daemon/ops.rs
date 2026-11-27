//! The store operations the CLI asks of the daemon.
//!
//! The daemon speaks the node's own operations, not SQL (ADR-0023 §2): MCP
//! clients send MCP tool calls, and the CLI sends these. Each one is a store
//! call the CLI used to make directly; its result travels back as the same
//! typed value, so the CLI renders it exactly as before.
//!
//! [`execute`] is also what the CLI runs in-process, so both paths share one
//! implementation and one serialization round trip.

use crate::db::{Database, Store};
use crate::models::{EntityInput, MemoryAddInput, MemoryListInput, MemorySearchInput};
use crate::{db::queries, entity, entity_duplicates, session_ops, stats, wiki, wiki_import};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Complete durable inventory, with optional supplemental ranked retrieval.
    ImpactExport {
        query: Option<String>,
    },
    /// Dry-run normalise plan; see [`crate::decisions_backfill`]. Writes nothing.
    DecisionsNormalizePlan {
        project: Option<String>,
        /// Project folds (`--fold FROM=TO`, pack D83); absent from older
        /// clients, which then plan none.
        #[serde(default)]
        folds: Vec<crate::decisions_backfill::Fold>,
    },
    /// Carry out the normalise plan's `respell`, `retype` and `add_ref`
    /// rows, re-planned at apply time; see
    /// [`crate::decisions_backfill::apply_normalize_with`]. The only decisions op
    /// that writes besides `DecisionsImportApply`. It needs no authorization
    /// beyond the daemon token every
    /// op presents, as `Add`, `Resolve` and `TopicsApply` do. Claim rows
    /// are skipped, as every generic edit refuses them.
    DecisionsNormalizeApply {
        project: Option<String>,
        /// As on [`Op::DecisionsNormalizePlan`].
        #[serde(default)]
        folds: Vec<crate::decisions_backfill::Fold>,
    },
    /// Dry-run import plan for one `DECISIONS.md`, sent as text so the
    /// daemon never reads the client's paths. Writes nothing.
    DecisionsImportPlan {
        project: String,
        source: String,
        text: String,
        #[serde(default)]
        refs: crate::decisions_backfill::RefOptions,
    },
    /// Add the import plan's `new` entries as decisions and carry out the
    /// supersedes links the file states, re-planned at apply time; see
    /// [`crate::decisions_backfill::apply_import`]. Never overwrites a
    /// stored decision. Authorized as `DecisionsNormalizeApply` is.
    DecisionsImportApply {
        project: String,
        source: String,
        text: String,
        #[serde(default)]
        refs: crate::decisions_backfill::RefOptions,
    },
    /// One project's decisions as a markdown log; see [`crate::decisions_log`].
    DecisionsLog {
        project: String,
        include_superseded: bool,
    },
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
    /// Likely duplicate entities; see [`crate::entity_duplicates`].
    EntityDuplicates,
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
    /// Compile pending memories into wiki pages with the configured local
    /// model; see [`crate::wiki_compile`].
    WikiAutoCompile {
        max_batches: usize,
        allow_shrink: bool,
    },
    /// Close a decision or action item; see [`crate::resolve`].
    Resolve {
        memory_id: String,
        outcome: String,
        note: Option<String>,
    },
    /// Record a decision's follow-up review; see [`crate::review`].
    Review {
        memory_id: String,
        outcome: String,
        note: Option<String>,
    },
    Stats,
    /// Mute an entity, topic or tag; see [`crate::mute`].
    Mute {
        target: String,
        value: String,
        project: Option<String>,
    },
    /// Turn a mute off, keeping its record.
    Unmute {
        target: String,
        value: String,
        project: Option<String>,
    },
    /// Every mute record, on or off.
    Mutes,
    /// Check the dialog store against `memories`; see
    /// [`crate::dialog_reconcile`]. `full` re-hashes every blob.
    DialogsCheck {
        full: bool,
    },
    /// Rebuild every dialog into a scratch store and compare; see
    /// [`crate::dialog_rebuild`]. `scratch` absolute, or `None` for the
    /// default beside the database.
    DialogsRebuildCheck {
        scratch: Option<PathBuf>,
        keep: bool,
    },
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
    ImportSessions {
        dir: PathBuf,
    },
    Context {
        cwd: Option<PathBuf>,
        project: Option<String>,
        branch: Option<String>,
        prompt: Option<String>,
        budget: usize,
        #[serde(default)]
        hits_only: bool,
    },
    SessionTimeline {
        session_id: String,
    },
    /// A project brief; see [`crate::context_brief`].
    Brief {
        project: String,
        since: Option<String>,
        budget: usize,
    },
    /// Cluster refined memories into a seed run; see [`crate::topic_seed`].
    TopicsSeed,
    /// Name a seed run's nodes with the configured local model. Takes the
    /// database, like [`Op::WikiAutoCompile`].
    TopicsName {
        run: String,
        max_nodes: usize,
    },
    /// Write a named seed run's topics and memberships, or show the plan.
    TopicsApply {
        run: String,
        dry_run: bool,
    },
    /// Rebuild topic centroids and assign every unplaced memory; see
    /// [`crate::topic_assign`].
    TopicsAssign,
    /// The rebalance check; see [`crate::topic_curate::report`].
    TopicsReport,
    /// A tree change; see [`crate::topic_curate`].
    TopicsCurate {
        action: TopicAction,
        because: String,
    },
    /// Measure hierarchical routing against flat search; see
    /// [`crate::routing`]. Takes the database: the queries are embedded with
    /// no store handle held.
    RoutingEval {
        request: crate::routing::EvalRequest,
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

/// One change to the topic tree, by topic name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TopicAction {
    Merge { from: String, into: String },
    Rename { from: String, to: String },
    Move { topic: String, parent: String },
    Retire { topic: String },
}

/// Run an operation against `db`. Every operation but the three that call a
/// model runs under one store handle; [`Op::WikiAutoCompile`],
/// [`Op::TopicsName`] and [`Op::RoutingEval`] take the database, so the
/// handle, which holds the database lock, is not held while a model runs.
pub fn execute_on(db: &Database, op: &Op) -> OpReply {
    let result = match op {
        Op::WikiAutoCompile {
            max_batches,
            allow_shrink,
        } => {
            let options = crate::wiki_compile::AutoOptions {
                max_batches: *max_batches,
                allow_shrink: *allow_shrink,
                ..Default::default()
            };
            let wiki = crate::wiki_fs::Wiki::from_env();
            crate::wiki_llm::compile_with_configured_model(&wiki, db, &options)
                .and_then(|report| serde_json::to_value(report).map_err(|e| e.to_string()))
        }
        Op::TopicsName { run, max_nodes } => {
            crate::wiki_llm::name_topics_with_configured_model(db, run, *max_nodes)
                .and_then(|report| serde_json::to_value(report).map_err(|e| e.to_string()))
        }
        Op::RoutingEval { request } => crate::routing::eval_on(db, request)
            .and_then(|outcome| serde_json::to_value(outcome).map_err(|e| e.to_string())),
        _ => return execute(&db.store(), op),
    };
    match result {
        Ok(value) => OpReply::Ok(value),
        Err(message) => OpReply::Err(message),
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
        Op::ImpactExport { query } => crate::export::impact_snapshot(store, query.as_deref()),
        Op::DecisionsNormalizePlan { project, folds } => {
            let folds = crate::decisions_backfill::Folds::new(folds.clone())?;
            to_value(
                crate::db::memories::Memories::new(store)
                    .all_live()
                    .map(|all| {
                        crate::decisions_backfill::plan_normalize_with(
                            &all,
                            project.as_deref(),
                            &folds,
                        )
                    }),
            )
        }
        Op::DecisionsNormalizeApply { project, folds } => {
            let folds = crate::decisions_backfill::Folds::new(folds.clone())?;
            to_value(crate::decisions_backfill::apply_normalize_with(
                store,
                project.as_deref(),
                &folds,
            ))
        }
        Op::DecisionsImportPlan {
            project,
            source,
            text,
            refs,
        } => to_value(
            crate::db::memories::Memories::new(store)
                .live_of_type("decision")
                .map(|all| {
                    let log = crate::decisions_backfill::parse_log_with(text, refs);
                    crate::decisions_backfill::plan_import(&log, project, source, &all)
                }),
        ),
        Op::DecisionsImportApply {
            project,
            source,
            text,
            refs,
        } => to_value(crate::decisions_backfill::apply_import(
            store,
            &crate::decisions_backfill::parse_log_with(text, refs),
            project,
            source,
        )),
        Op::DecisionsLog {
            project,
            include_superseded,
        } => to_value(
            crate::decisions_log::export(
                store,
                &crate::decisions_log::DecisionsQuery {
                    project: project.clone(),
                    include_superseded: *include_superseded,
                },
            )
            .map(|log| log.markdown),
        ),
        Op::Search { input } => to_value(queries::search_memories(store, input)),
        Op::Add { input } => to_value(queries::add_memory(store, input.clone())),
        Op::List { input } => to_value(queries::list_memories(store, input)),
        Op::Get { id } => to_value(queries::get_memory_by_id(store, id)),
        Op::UpsertEntity { input } => to_value(
            crate::topic::refuse_topic_name(&input.name)
                .and_then(|()| entity::upsert_entity(store, input)),
        ),
        Op::EntityDuplicates => to_value(entity_duplicates::entity_duplicates(store)),
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
        Op::WikiAutoCompile { .. } => {
            Err("a wiki auto compile needs the database: run it with execute_on".to_string())
        }
        Op::Resolve {
            memory_id,
            outcome,
            note,
        } => to_value(crate::resolve::resolve_memory(
            store,
            memory_id,
            outcome,
            note.as_deref(),
        )),
        Op::Review {
            memory_id,
            outcome,
            note,
        } => to_value(crate::review::record_review(
            store,
            memory_id,
            outcome,
            note.as_deref(),
        )),
        Op::Stats => to_value(stats::collect(store)),
        Op::Mute {
            target,
            value,
            project,
        } => to_value(
            crate::mute::Target::parse(target)
                .and_then(|t| crate::mute::mute(store, t, value, project.as_deref())),
        ),
        Op::Unmute {
            target,
            value,
            project,
        } => to_value(
            crate::mute::Target::parse(target)
                .and_then(|t| crate::mute::unmute(store, t, value, project.as_deref())),
        ),
        Op::Mutes => to_value(crate::mute::list(store)),
        Op::DialogsCheck { full } => {
            let depth = if *full {
                crate::dialog_reconcile::Depth::Full
            } else {
                crate::dialog_reconcile::Depth::Quick
            };
            to_value(crate::dialog_reconcile::check(store, depth))
        }
        Op::DialogsRebuildCheck { scratch, keep } => to_value(crate::dialog_rebuild::check(
            store,
            scratch.as_deref(),
            *keep,
        )),
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
        Op::ImportSessions { dir } => {
            session_ops::import_sessions(store, dir).map_err(|e| e.to_string())
        }
        Op::Context {
            cwd,
            project,
            branch,
            prompt,
            budget,
            hits_only,
        } => session_ops::context(
            store,
            &session_ops::ContextArgs {
                cwd: cwd.as_deref(),
                project: project.as_deref(),
                branch: branch.as_deref(),
                prompt: prompt.as_deref(),
                budget: *budget,
                hits_only: *hits_only,
            },
        )
        .map_err(|e| e.to_string()),
        Op::Brief {
            project,
            since,
            budget,
        } => {
            session_ops::brief(store, project, since.as_deref(), *budget).map_err(|e| e.to_string())
        }
        Op::SessionTimeline { session_id } => {
            session_ops::session_timeline(store, session_id).map_err(|e| e.to_string())
        }
        Op::TopicsSeed => {
            crate::topic_seed::seed(store, &Default::default()).and_then(|snapshot| {
                serde_json::to_value(seed_summary(&snapshot)).map_err(|e| e.to_string())
            })
        }
        Op::TopicsName { .. } => {
            Err("naming topics needs the database: run it with execute_on".to_string())
        }
        Op::RoutingEval { .. } => {
            Err("routing eval needs the database: run it with execute_on".to_string())
        }
        Op::TopicsApply { run, dry_run } => crate::topic_seed::apply(store, run, "human", *dry_run)
            .and_then(|report| serde_json::to_value(report).map_err(|e| e.to_string())),
        Op::TopicsAssign => crate::topic_assign::assign_pending(store)
            .and_then(|report| serde_json::to_value(report).map_err(|e| e.to_string())),
        Op::TopicsReport => crate::topic_curate::report(store)
            .and_then(|report| serde_json::to_value(report).map_err(|e| e.to_string())),
        Op::TopicsCurate { action, because } => {
            use crate::topic_curate as curate;
            match action {
                TopicAction::Merge { from, into } => curate::merge(store, from, into, because),
                TopicAction::Rename { from, to } => curate::rename(store, from, to, because),
                TopicAction::Move { topic, parent } => {
                    curate::move_to(store, topic, parent, because)
                }
                TopicAction::Retire { topic } => curate::retire(store, topic, because),
            }
            .and_then(|outcome| serde_json::to_value(outcome).map_err(|e| e.to_string()))
        }
        Op::Status | Op::Shutdown => Err("only a daemon answers status and shutdown".into()),
    }
}

/// A seed run without its member lists, which can be tens of thousands of
/// ids: the run id, the node count, and the size of each level.
fn seed_summary(snapshot: &crate::topic_seed::Snapshot) -> Value {
    let mut levels: std::collections::BTreeMap<usize, usize> = Default::default();
    for node in &snapshot.nodes {
        *levels.entry(node.depth).or_default() += 1;
    }
    serde_json::json!({
        "run": snapshot.run,
        "nodes": snapshot.nodes.len(),
        "memories": snapshot.nodes.iter().filter(|n| n.depth == 1).map(|n| n.size).sum::<usize>(),
        "nodes_per_level": levels,
    })
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
            ..Default::default()
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
    fn resolve_answers_with_the_resolved_memory_or_an_error() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let resolve = |id: &str, outcome: &str| {
            execute(
                &store,
                &Op::Resolve {
                    memory_id: id.into(),
                    outcome: outcome.into(),
                    note: None,
                },
            )
        };
        // A plain note tracks no outcome, so resolving it is refused.
        let note = add(&store, "just a note");
        assert!(matches!(resolve(&note.id, "done"), OpReply::Err(_)));
        let missing: Option<Memory> = resolve("mem_ghost", "done").into_result().unwrap();
        assert!(missing.is_none());

        let input = serde_json::from_value(serde_json::json!({
            "content": "ship it", "metadata": { "rationale": "r" }
        }))
        .unwrap();
        let decision = crate::db::queries::add_memory_with(
            &store,
            input,
            &crate::kinds::StructuredFields {
                memory_type: Some("decision".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let done: Option<Memory> = resolve(&decision.id, "done").into_result().unwrap();
        assert_eq!(done.unwrap().outcome.as_deref(), Some("done"));

        // Executed is not the same as held up: the review is its own call.
        let review = |id: &str, outcome: &str| {
            execute(
                &store,
                &Op::Review {
                    memory_id: id.into(),
                    outcome: outcome.into(),
                    note: Some("still right".into()),
                },
            )
        };
        assert!(decision.metadata["review"]["due"].is_string());
        let held: Option<Memory> = review(&decision.id, "held").into_result().unwrap();
        let held = held.unwrap();
        assert_eq!(held.metadata["review"]["outcome"], "held");
        assert_eq!(held.outcome.as_deref(), Some("done"));
        assert!(matches!(review(&note.id, "held"), OpReply::Err(_)));
        assert!(matches!(review(&decision.id, "fine"), OpReply::Err(_)));
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
